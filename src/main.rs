use bytes::Bytes;
use clap::{Arg, ArgAction, Command};
use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode, Uri};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use page::Page;
use probly_search::score::bm25;
use probly_search::Index;
use std::collections::{HashMap, HashSet};
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use store::{PageStore, StoredPage};
use texting_robots::{get_robots_url, Robot};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;
use url::Url;

mod page;
mod store;
mod tokenizer;

use tokenizer::tokenize;

const ROBOTS_USER_AGENT: &str = "rseek";
const HTTP_USER_AGENT: &str = concat!(
    "rseek/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/arazmj/rseek)"
);

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

fn extract_stored_title(p: &StoredPage) -> Vec<&str> {
    p.title.as_deref().into_iter().collect()
}

fn extract_stored_content(p: &StoredPage) -> Vec<&str> {
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

#[derive(Clone)]
struct CrawlState {
    index: Arc<Mutex<Index<usize>>>,
    page_count: Arc<AtomicUsize>,
    store: Arc<PageStore>,
}

#[derive(Clone)]
struct CrawlConfig {
    max_pages: usize,
    seed_origin: String,
    allow_cross_origin: bool,
    timeout_secs: u64,
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

fn is_allowed(robot: Option<&Robot>, url: &str) -> bool {
    match robot {
        Some(robot) => robot.allowed(url),
        None => true,
    }
}

#[tracing::instrument(skip(client))]
async fn fetch_robots(
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    seed_url: &str,
    timeout_secs: u64,
) -> Option<Robot> {
    let robots_url = match get_robots_url(seed_url) {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(seed_url, error = %error, "could not determine robots.txt URL; allowing crawl");
            return None;
        }
    };
    let uri = match robots_url.parse::<Uri>() {
        Ok(uri) => uri,
        Err(error) => {
            tracing::warn!(robots_url, error = %error, "invalid robots.txt URL; allowing crawl");
            return None;
        }
    };
    let request = match Request::builder()
        .uri(uri)
        .header(hyper::header::USER_AGENT, HTTP_USER_AGENT)
        .body(Empty::new())
    {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(robots_url, error = %error, "could not build robots.txt request; allowing crawl");
            return None;
        }
    };

    let response = match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        client.request(request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!(robots_url, error = %error, "could not fetch robots.txt; allowing crawl");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                robots_url,
                timeout_secs,
                "robots.txt request timed out; allowing crawl"
            );
            return None;
        }
    };

    if response.status() == StatusCode::NOT_FOUND {
        tracing::debug!(robots_url, "robots.txt not found; allowing crawl");
        return None;
    }
    if !response.status().is_success() {
        tracing::warn!(robots_url, status = %response.status(), "robots.txt request failed; allowing crawl");
        return None;
    }

    let body = match tokio::time::timeout(Duration::from_secs(timeout_secs), response.collect())
        .await
    {
        Ok(Ok(body)) => body.to_bytes(),
        Ok(Err(error)) => {
            tracing::warn!(robots_url, error = %error, "could not read robots.txt; allowing crawl");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                robots_url,
                timeout_secs,
                "robots.txt response timed out; allowing crawl"
            );
            return None;
        }
    };

    match Robot::new(ROBOTS_USER_AGENT, &body) {
        Ok(robot) => Some(robot),
        Err(error) => {
            tracing::warn!(robots_url, error = %error, "could not parse robots.txt; allowing crawl");
            None
        }
    }
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

#[tracing::instrument(skip(client, tx, state, config))]
async fn crawl_worker(
    url: String,
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    tx: mpsc::Sender<CrawlItem>,
    state: CrawlState,
    config: CrawlConfig,
) {
    match crawl_url(url.clone(), client, config.timeout_secs).await {
        Ok(page) => {
            tracing::info!(url = %url, links = page.hrefs.len(), "crawled page");

            let indexed_pages = {
                let mut index = state.index.lock().await;
                let page_id = state.page_count.load(Ordering::SeqCst);
                if !should_enqueue_links(page_id, config.max_pages) {
                    return;
                }

                let stored_page = StoredPage {
                    url: url.clone(),
                    title: page.title.clone(),
                    content: page.content.clone(),
                };
                if let Err(error) = state.store.append(&stored_page) {
                    tracing::error!(url = %url, error = %error, "failed to store crawled page");
                    return;
                }

                index.add_document(&[extract_title, extract_content], tokenize, page_id, &page);
                state.page_count.fetch_add(1, Ordering::SeqCst) + 1
            };

            if !should_enqueue_links(indexed_pages, config.max_pages) {
                return;
            }

            for link in page.hrefs {
                if let Some(link) = make_absolute_url(&url, &link) {
                    if let Some(normalized_link) = normalize_url(&link) {
                        if config.allow_cross_origin
                            || is_same_origin(&normalized_link, &config.seed_origin)
                        {
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
                    Arg::new("ignore-robots")
                        .help("Ignore robots.txt rules")
                        .long("ignore-robots")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("timeout")
                        .help("Request timeout in seconds")
                        .short('t')
                        .long("timeout")
                        .default_value("10")
                        .value_parser(clap::value_parser!(u64)),
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
            let normalized_seed = normalize_url(url).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("seed URL must be an absolute HTTP(S) URL: {url}"),
                )
            })?;
            let seed_origin = Url::parse(&normalized_seed)?.origin().ascii_serialization();
            let allow_cross_origin = sub_matches.get_flag("allow-cross-origin");
            let ignore_robots = sub_matches.get_flag("ignore-robots");
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
            let (tx, mut rx) = mpsc::channel::<CrawlItem>(100);
            let visited = Arc::new(Mutex::new(HashSet::new()));
            let semaphore = Arc::new(Semaphore::new(concurrency));
            let page_count = Arc::new(AtomicUsize::new(0));
            let state = CrawlState {
                index: Arc::new(Mutex::new(Index::<usize>::new(2))),
                page_count: page_count.clone(),
                store,
            };
            let config = CrawlConfig {
                max_pages,
                seed_origin,
                allow_cross_origin,
                timeout_secs,
            };
            let mut robots_by_origin = HashMap::<String, Option<Robot>>::new();
            let mut workers = JoinSet::new();
            let mut shutdown_requested = false;
            let ctrl_c = tokio::signal::ctrl_c();
            tokio::pin!(ctrl_c);

            tx.send(CrawlItem {
                url: normalized_seed,
                tx: tx.clone(),
            })
            .await?;
            drop(tx);

            loop {
                tokio::select! {
                    signal = &mut ctrl_c => {
                        if let Err(error) = signal {
                            tracing::error!(error = %error, "failed to listen for Ctrl-C");
                        }
                        tracing::info!("received Ctrl-C; stopping URL scheduling");
                        shutdown_requested = true;
                        rx.close();
                        break;
                    }
                    maybe_item = rx.recv() => {
                        let Some(CrawlItem { url, tx }) = maybe_item else {
                            break;
                        };
                        let Some(url) = normalize_url(&url) else {
                            continue;
                        };

                        if !should_enqueue_links(page_count.load(Ordering::SeqCst), max_pages) {
                            continue;
                        }

                        if !ignore_robots {
                            let origin = Url::parse(&url)?.origin().ascii_serialization();
                            if !robots_by_origin.contains_key(&origin) {
                                let robot = fetch_robots(client.clone(), &url, timeout_secs).await;
                                robots_by_origin.insert(origin.clone(), robot);
                            }
                            if !is_allowed(robots_by_origin.get(&origin).and_then(Option::as_ref), &url) {
                                tracing::info!(url, "skipping URL disallowed by robots.txt");
                                continue;
                            }
                        }

                        if is_visited(&url, &visited).await {
                            continue;
                        }

                        let semaphore = semaphore.clone();
                        let client = client.clone();
                        let state = state.clone();
                        let config = config.clone();

                        workers.spawn(async move {
                            let Ok(permit) = semaphore.acquire_owned().await else {
                                return;
                            };
                            crawl_worker(
                                url,
                                client,
                                tx,
                                state,
                                config,
                            )
                            .await;
                            drop(permit);
                        });
                    }
                    result = workers.join_next(), if !workers.is_empty() => {
                        if let Some(Err(error)) = result {
                            tracing::error!(error = %error, "crawl worker failed");
                        }
                    }
                }
            }

            if shutdown_requested {
                let drain_result = tokio::time::timeout(Duration::from_secs(5), async {
                    while let Some(result) = workers.join_next().await {
                        if let Err(error) = result {
                            tracing::error!(error = %error, "crawl worker failed during shutdown");
                        }
                    }
                })
                .await;

                if drain_result.is_err() {
                    tracing::warn!("timed out waiting for crawl workers; aborting remaining work");
                    workers.abort_all();
                    while workers.join_next().await.is_some() {}
                }

                tracing::info!(
                    pages = page_count.load(Ordering::SeqCst),
                    "shutdown summary"
                );
            } else {
                while let Some(result) = workers.join_next().await {
                    if let Err(error) = result {
                        tracing::error!(error = %error, "crawl worker failed");
                    }
                }
            }

            tracing::info!(
                pages = page_count.load(Ordering::SeqCst),
                "crawling completed"
            );
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
                index.add_document(
                    &[extract_stored_title, extract_stored_content],
                    tokenize,
                    id,
                    page,
                );
            }

            let result = index.query(query, &mut bm25::new(), tokenize, &[1., 1.]);
            tracing::info!(path = ?store_path, results = result.len(), "searching stored pages");
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
    timeout_secs: u64,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    tracing::debug!(%uri, "fetching page");
    let uri_display = uri.to_string();
    let req = Request::builder()
        .uri(uri)
        .header(hyper::header::USER_AGENT, HTTP_USER_AGENT)
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

    #[test]
    fn robots_rules_allow_and_disallow_expected_paths() {
        let robot = Robot::new(
            ROBOTS_USER_AGENT,
            b"User-agent: rseek\nDisallow: /private\nAllow: /private/public\n",
        )
        .unwrap();

        assert!(is_allowed(Some(&robot), "https://example.com/"));
        assert!(!is_allowed(
            Some(&robot),
            "https://example.com/private/secret"
        ));
        assert!(is_allowed(
            Some(&robot),
            "https://example.com/private/public"
        ));
        assert!(is_allowed(None, "https://example.com/private"));
    }

    #[test]
    fn extractor_helpers_return_page_fields_for_indexing() {
        let titled = Page {
            title: Some("Example Title".to_string()),
            content: "Example body".to_string(),
            hrefs: vec![],
        };
        let untitled = Page {
            title: None,
            content: "Untitled body".to_string(),
            hrefs: vec![],
        };

        assert_eq!(extract_title(&titled), vec!["Example Title"]);
        assert!(extract_title(&untitled).is_empty());
        assert_eq!(extract_content(&titled), vec!["Example body"]);
    }

    #[test]
    fn tokenizer_splits_on_spaces() {
        let tokens = tokenize("rust search tool")
            .into_iter()
            .map(|token| token.into_owned())
            .collect::<Vec<_>>();

        assert_eq!(tokens, vec!["rust", "search", "tool"]);
    }

    #[test]
    fn make_absolute_url_handles_absolute_scheme_relative_root_and_relative_links() {
        let base = "https://example.com/docs/index.html";

        assert_eq!(
            make_absolute_url(base, "https://rust-lang.org"),
            Some("https://rust-lang.org/".to_string())
        );
        assert_eq!(
            make_absolute_url(base, "//cdn.example.com/app.js"),
            Some("https://cdn.example.com/app.js".to_string())
        );
        assert_eq!(
            make_absolute_url(base, "/about"),
            Some("https://example.com/about".to_string())
        );
        assert_eq!(
            make_absolute_url(base, "guide/start.html"),
            Some("https://example.com/docs/guide/start.html".to_string())
        );
        assert_eq!(make_absolute_url("not a url", "/about"), None);
    }
}
