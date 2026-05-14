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
use std::error::Error;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing_subscriber::EnvFilter;
use url::Url;

mod page;

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

struct CrawlItem {
    url: String,
    tx: mpsc::Sender<CrawlItem>,
}

fn should_enqueue_links(indexed_pages: usize, max_pages: usize) -> bool {
    indexed_pages < max_pages
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

#[tracing::instrument(skip(client, tx, index))]
async fn crawl_worker(
    url: String,
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    tx: mpsc::Sender<String>,
    index: Arc<Mutex<Index<usize>>>,
    page_id: usize,
) {
    match crawl_url(url.clone(), client).await {
        Ok(page) => {
            tracing::info!(url = %url, links = page.hrefs.len(), "crawled page");

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
                .arg(
                    Arg::new("max-pages")
                        .help("Maximum number of pages to index")
                        .short('m')
                        .long("max-pages")
                        .default_value("100")
                )
        )
        .subcommand(
            Command::new("search")
                .about("Search through crawled content")
                .arg(
                    Arg::new("query")
                        .help("The search query")
                        .required(true)
                        .index(1),
                ),
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
            let max_pages = sub_matches.get_one::<String>("max-pages")
                .unwrap()
                .parse::<usize>()
                .unwrap_or(100);

            // Create a new HTTP client with HTTPS support
            let https = HttpsConnector::new();
            let client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>> =
                Client::builder(TokioExecutor::new()).build(https);

            // Setup crawling infrastructure
            let (tx, mut rx) = mpsc::channel::<CrawlItem>(100);
            let visited = Arc::new(Mutex::new(HashSet::new()));
            let semaphore = Arc::new(Semaphore::new(concurrency));
            let index = Arc::new(Mutex::new(Index::<usize>::new(2)));
            let page_count = Arc::new(AtomicUsize::new(0));

            // Send the initial URL, then drop the original sender so the receiver can close.
            tx.send(CrawlItem { url: url.clone(), tx: tx.clone() }).await?;
            drop(tx);

            // Process URLs
            while let Some(CrawlItem { url, tx }) = rx.recv().await {
                let visited = visited.clone();
                let semaphore = semaphore.clone();
                let client = client.clone();
                let index = index.clone();
                let page_count = page_count.clone();

                tokio::spawn(async move {
                    if !should_enqueue_links(page_count.load(Ordering::SeqCst), max_pages) {
                        return;
                    }

                    if is_visited(&url, &visited).await {
                        return;
                    }

                    let Ok(permit) = semaphore.acquire_owned().await else {
                        return;
                    };

                    match crawl_url(url.clone(), client).await {
                        Ok(page) => {
                            tracing::info!(url = %url, links = page.hrefs.len(), "crawled page");

                            let indexed_pages = {
                                let mut index = index.lock().await;
                                let doc_id = page_count.load(Ordering::SeqCst);

                                if !should_enqueue_links(doc_id, max_pages) {
                                    return;
                                }

                                index.add_document(
                                    &[extract_title, extract_content],
                                    tokenizer,
                                    doc_id,
                                    &page
                                );

                                let previous = page_count.fetch_add(1, Ordering::SeqCst);
                                debug_assert_eq!(previous, doc_id);
                                previous + 1
                            };

                            if !should_enqueue_links(indexed_pages, max_pages) {
                                return;
                            }

                            for link in page.hrefs {
                                let link = make_absolute_url(&url, &link);
                                if let Some(link) = link {
                                    tx.send(CrawlItem { url: link, tx: tx.clone() }).await.ok();
                                }
                            }
                        }
                        Err(e) => tracing::error!(url = %url, error = %e, "error crawling"),
                    }

                    drop(permit);
                });
            }

            tracing::info!(
                pages = page_count.load(Ordering::SeqCst),
                "crawling completed"
            );
        }
        Some(("search", sub_matches)) => {
            let query = sub_matches.get_one::<String>("query").unwrap();

            // TODO: Load the index
            let index = Index::<usize>::new(2);

            // Search through the index
            let result = index.query(query, &mut bm25::new(), tokenizer, &[1., 1.]);
            println!("Search results:");
            for (i, res) in result.iter().enumerate() {
                println!("{}. Score: {}", i + 1, res.score);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_enqueue_links_until_page_limit_is_reached() {
        assert!(should_enqueue_links(0, 1));
        assert!(should_enqueue_links(99, 100));
        assert!(!should_enqueue_links(100, 100));
        assert!(!should_enqueue_links(101, 100));
        assert!(!should_enqueue_links(0, 0));
    }
}
