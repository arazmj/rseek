use bytes::Bytes;
use clap::{Arg, ArgAction, Command};
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
use std::sync::Arc;
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

pub fn normalize_url(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }

    url.set_fragment(None);

    let default_port = match url.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    if url.port_or_known_default() == default_port && url.port().is_some() {
        url.set_port(None).ok()?;
    }

    let mut normalized = url.to_string();
    if url.path() == "/" && url.query().is_none() {
        normalized.pop();
    }

    Some(normalized)
}

fn is_same_origin(url: &str, seed_host: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| host.eq_ignore_ascii_case(seed_host))
        })
        .unwrap_or(false)
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
    seed_host: String,
    allow_cross_origin: bool,
) {
    match crawl_url(url.clone(), client).await {
        Ok(page) => {
            tracing::info!(url = %url, links = page.hrefs.len(), "crawled page");

            let mut index = index.lock().await;
            index.add_document(&[extract_title, extract_content], tokenizer, page_id, &page);
            drop(index);

            for link in page.hrefs {
                if let Some(link) = make_absolute_url(&url, &link) {
                    if let Some(normalized_link) = normalize_url(&link) {
                        if allow_cross_origin || is_same_origin(&normalized_link, &seed_host) {
                            tracing::debug!(url = %url, link = %normalized_link, "queueing discovered link");
                            if tx.send(normalized_link).await.is_err() {
                                tracing::warn!(url = %url, "failed to queue discovered link because crawler channel is closed");
                            }
                        }
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
                    Arg::new("allow-cross-origin")
                        .help("Allow crawling links outside the seed URL origin")
                        .long("allow-cross-origin")
                        .action(ArgAction::SetTrue),
                ),
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
            let seed_url = Url::parse(url).unwrap();
            let seed_host = seed_url.host_str().unwrap().to_owned();
            let allow_cross_origin = sub_matches.get_flag("allow-cross-origin");
            let concurrency = sub_matches
                .get_one::<String>("concurrency")
                .unwrap()
                .parse::<usize>()
                .unwrap_or(10);

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
            tx.send(normalize_url(url).unwrap()).await?;

            // Process URLs
            while let Some(url) = rx.recv().await {
                let tx = tx.clone();
                let visited = visited.clone();
                let semaphore = semaphore.clone();
                let client = client.clone();
                let index = index.clone();
                let seed_host = seed_host.clone();

                let Some(url) = normalize_url(&url) else {
                    continue;
                };

                if !is_visited(&url, &visited).await {
                    let permit = semaphore.acquire_owned().await?;

                    tokio::spawn(async move {
                        crawl_worker(url, client, tx, index, page_count, seed_host, allow_cross_origin).await;
                        drop(permit);
                    });
                }
                page_count += 1;
            }

            tracing::info!("Crawling completed. Indexed {} pages.", page_count);
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
    use super::normalize_url;

    #[test]
    fn normalizes_scheme_host_fragment_default_ports_and_root_path() {
        assert_eq!(
            normalize_url("https://Example.COM/Foo#bar"),
            Some("https://example.com/Foo".to_string())
        );
        assert_eq!(
            normalize_url("http://example.com:80/x"),
            Some("http://example.com/x".to_string())
        );
        assert_eq!(
            normalize_url("https://example.com:443/y"),
            Some("https://example.com/y".to_string())
        );
        assert_eq!(
            normalize_url("https://example.com/"),
            Some("https://example.com".to_string())
        );
    }

    #[test]
    fn rejects_non_http_urls() {
        assert_eq!(normalize_url("mailto:foo@bar.com"), None);
        assert_eq!(normalize_url("javascript:alert(1)"), None);
        assert_eq!(normalize_url("not a url"), None);
    }
}
