use bytes::Bytes;
use clap::{Arg, Command};
use http_body_util::{BodyExt, Empty};
use hyper::{Request, Uri};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use page::Page;
use probly_search::score::bm25;
use probly_search::Index;
use scraper::{Html, Selector};
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use store::{PageStore, StoredPage};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing_subscriber::EnvFilter;
use url::Url;

mod page;
mod store;

struct IndexablePage {
    title: Option<String>,
    content: String,
}

fn extract_title(p: &Page) -> Vec<&str> {
    if let Some(title) = &p.title {
        vec![title]
    } else {
        vec![]
    }
}

fn extract_content(p: &Page) -> Vec<&str> {
    vec![&p.content]
}

fn extract_indexable_title(p: &IndexablePage) -> Vec<&str> {
    if let Some(title) = &p.title {
        vec![title]
    } else {
        vec![]
    }
}

fn extract_indexable_content(p: &IndexablePage) -> Vec<&str> {
    vec![&p.content]
}

fn default_store_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("rseek")
        .join("pages.jsonl")
}

fn store_arg() -> Arg {
    Arg::new("store")
        .help("Path to the JSONL page store")
        .short('s')
        .long("store")
        .value_name("PATH")
        .value_parser(clap::value_parser!(PathBuf))
}

// A white space tokenizer
fn tokenizer(s: &str) -> Vec<Cow<str>> {
    s.split(' ').map(Cow::from).collect::<Vec<_>>()
}

async fn is_visited(url: &str, visited: &Arc<Mutex<HashSet<String>>>) -> bool {
    let mut visited = visited.lock().await;
    if visited.contains(url) {
        true
    } else {
        visited.insert(url.to_string());
        false
    }
}

fn parse_links(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a[href]").unwrap();

    document
        .select(&selector)
        .filter_map(|element| element.value().attr("href"))
        .filter(|href| href.starts_with("http"))
        .map(String::from)
        .collect()
}

fn make_absolute_url(base: &str, href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        Some(href.to_string())
    } else if href.starts_with("//") {
        Some(format!("https:{}", href))
    } else if href.starts_with('/') {
        let base_url = Url::parse(base).ok()?;
        let scheme = base_url.scheme();
        let host = base_url.host_str()?;
        Some(format!("{}://{}{}", scheme, host, href))
    } else {
        let base_url = Url::parse(base).ok()?;
        base_url.join(href).ok().map(|u| u.to_string())
    }
}

#[tracing::instrument(skip(client))]
async fn crawl_url(
    url: String,
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
) -> Result<Page, Box<dyn Error + Send + Sync>> {
    let uri = url.parse::<Uri>()?;
    let html = fetch_page(client, uri).await?;
    Ok(Page::new(html))
}

#[tracing::instrument(skip(client, tx, index, store))]
async fn crawl_worker(
    url: String,
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    tx: mpsc::Sender<String>,
    index: Arc<Mutex<Index<usize>>>,
    page_id: usize,
    store: Arc<PageStore>,
) {
    match crawl_url(url.clone(), client).await {
        Ok(page) => {
            tracing::info!(url = %url, links = page.hrefs.len(), "crawled page");

            let stored_page = StoredPage {
                url: url.clone(),
                title: page.title.clone(),
                content: page.content.clone(),
            };
            if let Err(err) = store.append(&stored_page) {
                tracing::error!(url = %url, error = %err, "error storing page");
            }

            let mut index = index.lock().await;
            index.add_document(&[extract_title, extract_content], tokenizer, page_id, &page);
            drop(index);

            for link in page.hrefs {
                if let Some(link) = make_absolute_url(&url, &link) {
                    tracing::debug!(url = %url, link = %link, "queueing discovered link");
                    if tx.send(link).await.is_err() {
                        tracing::warn!(url = %url, "failed to queue discovered link because crawler channel is closed");
                    }
                }
            }
        }
        Err(error) => tracing::error!(url = %url, error = %error, "failed to crawl page"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let matches = Command::new("rseek")
        .version("1.0")
        .about("Web crawler and search tool")
        .subcommand_negates_reqs(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("crawl")
                .about("Crawl a webpage and extract its content")
                .arg(
                    Arg::new("url")
                        .help("The seed URL to crawl")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::new("concurrency")
                        .help("Number of concurrent requests")
                        .short('c')
                        .long("concurrency")
                        .default_value("10"),
                )
                .arg(store_arg()),
        )
        .subcommand(
            Command::new("search")
                .about("Search through crawled content")
                .arg(
                    Arg::new("query")
                        .help("The search query")
                        .required(true)
                        .index(1),
                )
                .arg(store_arg()),
        )
        .get_matches();

    match matches.subcommand() {
        Some(("crawl", sub_matches)) => {
            let url = sub_matches.get_one::<String>("url").unwrap();
            let concurrency = sub_matches
                .get_one::<String>("concurrency")
                .unwrap()
                .parse::<usize>()
                .unwrap_or(10);
            let store_path = sub_matches
                .get_one::<PathBuf>("store")
                .cloned()
                .unwrap_or_else(default_store_path);
            let store = Arc::new(PageStore::open(store_path.clone())?);
            tracing::info!(path = ?store_path, "storing crawled pages");

            // Create a new HTTP client with HTTPS support
            let https = HttpsConnector::new();
            let client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>> =
                Client::builder(TokioExecutor::new()).build(https);

            // Setup crawling infrastructure
            let (tx, mut rx) = mpsc::channel(100);
            let visited = Arc::new(Mutex::new(HashSet::new()));
            let semaphore = Arc::new(Semaphore::new(concurrency));
            let index = Arc::new(Mutex::new(Index::<usize>::new(2)));
            let mut page_count = 0;

            // Send the initial URL
            tx.send(url.clone()).await?;

            // Process URLs
            while let Some(url) = rx.recv().await {
                let tx = tx.clone();
                let visited = visited.clone();
                let semaphore = semaphore.clone();
                let client = client.clone();
                let index = index.clone();
                let store = store.clone();

                if !is_visited(&url, &visited).await {
                    let permit = semaphore.acquire_owned().await?;

                    tokio::spawn(async move {
                        crawl_worker(url, client, tx, index, page_count, store).await;
                        drop(permit);
                    });
                }
                page_count += 1;
            }

            tracing::info!(page_count, "crawling completed");
        }
        Some(("search", sub_matches)) => {
            let query = sub_matches.get_one::<String>("query").unwrap();
            let store_path = sub_matches
                .get_one::<PathBuf>("store")
                .cloned()
                .unwrap_or_else(default_store_path);
            let pages = PageStore::read_all(&store_path)?;
            let mut index = Index::<usize>::new(2);

            for (id, page) in pages.iter().enumerate() {
                let indexable = IndexablePage {
                    title: page.title.clone(),
                    content: page.content.clone(),
                };
                index.add_document(
                    &[extract_indexable_title, extract_indexable_content],
                    tokenizer,
                    id,
                    &indexable,
                );
            }

            // Search through the index
            let result = index.query(query, &mut bm25::new(), tokenizer, &[1., 1.]);
            tracing::info!(path = ?store_path, "searching stored pages");
            for (i, res) in result.iter().enumerate() {
                if let Some(page) = pages.get(res.key) {
                    tracing::info!(
                        position = i + 1,
                        url = %page.url,
                        title = page.title.as_deref().unwrap_or("<untitled>"),
                        score = res.score,
                        "search result"
                    );
                }
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}

#[tracing::instrument(skip(client))]
async fn fetch_page(
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    uri: Uri,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    tracing::debug!(%uri, "fetching page");
    let req = Request::builder().uri(uri).body(Empty::new())?;

    // Send the request and get the response
    let res = client.request(req).await?;

    // Get the response body and convert to string
    let body = res.collect().await?.to_bytes();
    let content = String::from_utf8(body.to_vec())?;
    Ok(content)
}
