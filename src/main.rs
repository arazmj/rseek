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
use std::collections::HashSet;
use std::error::Error;
use std::io;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing_subscriber::EnvFilter;
use url::Url;

mod page;
mod tokenizer;

use tokenizer::tokenize;

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

fn make_absolute_url(base: &str, href: &str) -> Option<String> {
    Url::parse(base).ok()?.join(href).ok().map(Into::into)
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

fn is_same_origin(url: &str, seed_origin: &str) -> bool {
    Url::parse(url)
        .ok()
        .map(|url| url.origin().ascii_serialization() == seed_origin)
        .unwrap_or(false)
}

#[tracing::instrument(skip(client))]
async fn crawl_url(
    url: String,
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    timeout_secs: u64,
) -> Result<Page, Box<dyn Error + Send + Sync>> {
    let uri = url.parse::<Uri>()?;
    let html = fetch_page(client, uri, timeout_secs).await?;
    Ok(Page::new(html))
}

#[tracing::instrument(skip(client, tx, index, page_count))]
async fn crawl_worker(
    url: String,
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    tx: mpsc::Sender<CrawlItem>,
    index: Arc<Mutex<Index<usize>>>,
    page_count: Arc<AtomicUsize>,
    max_pages: usize,
    seed_origin: String,
    allow_cross_origin: bool,
    timeout_secs: u64,
) {
    match crawl_url(url.clone(), client, timeout_secs).await {
        Ok(page) => {
            tracing::info!(url = %url, links = page.hrefs.len(), "crawled page");

            let indexed_pages = {
                let mut index = index.lock().await;
                let page_id = page_count.load(Ordering::SeqCst);
                if !should_enqueue_links(page_id, max_pages) {
                    return;
                }

                index.add_document(&[extract_title, extract_content], tokenize, page_id, &page);
                page_count.fetch_add(1, Ordering::SeqCst) + 1
            };

            if !should_enqueue_links(indexed_pages, max_pages) {
                return;
            }

            for link in page.hrefs {
                if let Some(link) = make_absolute_url(&url, &link) {
                    if let Some(normalized_link) = normalize_url(&link) {
                        if allow_cross_origin || is_same_origin(&normalized_link, &seed_origin) {
                            tracing::debug!(url = %url, link = %normalized_link, "queueing discovered link");
                            let item = CrawlItem {
                                url: normalized_link,
                                tx: tx.clone(),
                            };
                            if tx.send(item).await.is_err() {
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
                    Arg::new("max-pages")
                        .help("Maximum number of pages to index")
                        .short('m')
                        .long("max-pages")
                        .default_value("100")
                        .value_parser(clap::value_parser!(usize)),
                )
                .arg(
                    Arg::new("allow-cross-origin")
                        .help("Allow crawling links outside the seed URL origin")
                        .long("allow-cross-origin")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("timeout")
                        .help("Request timeout in seconds")
                        .short('t')
                        .long("timeout")
                        .default_value("10")
                        .value_parser(clap::value_parser!(u64)),
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
            let normalized_seed = normalize_url(url).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("seed URL must be an absolute HTTP(S) URL: {url}"),
                )
            })?;
            let seed_origin = Url::parse(&normalized_seed)?.origin().ascii_serialization();
            let allow_cross_origin = sub_matches.get_flag("allow-cross-origin");
            let concurrency = sub_matches
                .get_one::<String>("concurrency")
                .unwrap()
                .parse::<usize>()
                .unwrap_or(10);
            if concurrency == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "concurrency must be at least 1",
                )
                .into());
            }
            let max_pages = sub_matches
                .get_one::<usize>("max-pages")
                .copied()
                .unwrap_or(100);
            let timeout_secs = sub_matches.get_one::<u64>("timeout").copied().unwrap_or(10);

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

            tx.send(CrawlItem {
                url: normalized_seed,
                tx: tx.clone(),
            })
            .await?;
            drop(tx);

            while let Some(CrawlItem { url, tx }) = rx.recv().await {
                let Some(url) = normalize_url(&url) else {
                    continue;
                };

                if !should_enqueue_links(page_count.load(Ordering::SeqCst), max_pages)
                    || is_visited(&url, &visited).await
                {
                    continue;
                }

                let semaphore = semaphore.clone();
                let client = client.clone();
                let index = index.clone();
                let page_count = page_count.clone();
                let seed_origin = seed_origin.clone();

                tokio::spawn(async move {
                    let Ok(permit) = semaphore.acquire_owned().await else {
                        return;
                    };
                    crawl_worker(
                        url,
                        client,
                        tx,
                        index,
                        page_count,
                        max_pages,
                        seed_origin,
                        allow_cross_origin,
                        timeout_secs,
                    )
                    .await;
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
            let result = index.query(query, &mut bm25::new(), tokenize, &[1., 1.]);
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
    timeout_secs: u64,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    tracing::debug!(%uri, "fetching page");
    let uri_display = uri.to_string();
    let req = Request::builder()
        .uri(uri)
        .header(
            hyper::header::USER_AGENT,
            format!(
                "rseek/{} (+https://github.com/arazmj/rseek)",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .body(Empty::new())?;

    // Send the request and get the response
    let res = tokio::time::timeout(Duration::from_secs(timeout_secs), client.request(req))
        .await
        .map_err(|_| format!("request timed out after {}s", timeout_secs))??;
    if !res.status().is_success() {
        return Err(format!("HTTP {} for {}", res.status(), uri_display).into());
    }

    // Enforce the timeout while reading the response body as well as headers.
    let body = tokio::time::timeout(Duration::from_secs(timeout_secs), res.collect())
        .await
        .map_err(|_| format!("response body timed out after {timeout_secs}s"))??
        .to_bytes();
    Ok(decode_body(&body))
}

fn decode_body(bytes: &[u8]) -> String {
    // Sites often serve non-UTF-8 charsets; index garbled-but-mostly-correct
    // content rather than skipping the page entirely.
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_body_replaces_invalid_utf8() {
        let decoded = decode_body(&[0xC3, 0x28]);

        assert!(decoded.contains('\u{FFFD}'));
    }

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

    #[test]
    fn same_origin_includes_scheme_and_port() {
        let origin = "https://example.com";

        assert!(is_same_origin("https://example.com/page", origin));
        assert!(!is_same_origin("http://example.com/page", origin));
        assert!(!is_same_origin("https://example.com:8443/page", origin));
    }

    #[test]
    fn should_enqueue_links_until_page_limit_is_reached() {
        assert!(should_enqueue_links(0, 1));
        assert!(should_enqueue_links(99, 100));
        assert!(!should_enqueue_links(100, 100));
        assert!(!should_enqueue_links(101, 100));
        assert!(!should_enqueue_links(0, 0));
    }
}
