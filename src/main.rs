use std::borrow::Cow;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use hyper::{Request, Uri};
use http_body_util::{BodyExt, Empty};
use std::error::Error;
use bytes::Bytes;
use probly_search::Index;
use probly_search::score::bm25;
use page::Page;
use clap::{Command, Arg};
use tokio::sync::{mpsc, Semaphore, Mutex};
use std::collections::HashSet;
use std::sync::Arc;
use scraper::{Html, Selector};
use hyper_tls::HttpsConnector;
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

async fn crawl_url(url: String, client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>) -> Result<Page, Box<dyn Error + Send + Sync>> {
    let uri = url.parse::<Uri>()?;
    let html = fetch_page(client, uri).await?;
    Ok(Page::new(html))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
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
                        .index(1)
                )
                .arg(
                    Arg::new("concurrency")
                        .help("Number of concurrent requests")
                        .short('c')
                        .long("concurrency")
                        .default_value("10")
                )
        )
        .subcommand(
            Command::new("search")
                .about("Search through crawled content")
                .arg(
                    Arg::new("query")
                        .help("The search query")
                        .required(true)
                        .index(1)
                )
        )
        .get_matches();

    match matches.subcommand() {
        Some(("crawl", sub_matches)) => {
            let url = sub_matches.get_one::<String>("url").unwrap();
            let concurrency = sub_matches.get_one::<String>("concurrency")
                .unwrap()
                .parse::<usize>()
                .unwrap_or(10);

            // Create a new HTTP client with HTTPS support
            let https = HttpsConnector::new();
            let client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>> = Client::builder(TokioExecutor::new())
                .build(https);

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

                if !is_visited(&url, &visited).await {
                    let permit = semaphore.acquire_owned().await?;

                    tokio::spawn(async move {
                        match crawl_url(url.clone(), client).await {
                            Ok(page) => {
                                println!("Crawled: {}", url);
                                println!("Found {} links", page.hrefs.len());
                                
                                // Add page to search index
                                let mut index = index.lock().await;
                                index.add_document(
                                    &[extract_title, extract_content],
                                    tokenizer,
                                    page_count,
                                    &page
                                );


                                // Send new links to be crawled
                                for link in page.hrefs {
                                    let link = make_absolute_url(&url, &link);
                                    if let Some(link) = link {
                                        tx.send(link).await.ok();
                                    }
                                }
                            }
                            Err(e) => println!("Error crawling {}: {}", url, e),
                        }
                        drop(permit);
                    });
                }
                page_count += 1;
            }

            println!("Crawling completed. Indexed {} pages.", page_count);
        }
        Some(("search", sub_matches)) => {
            let query = sub_matches.get_one::<String>("query").unwrap();
            
            // TODO: Load the index
            let index = Index::<usize>::new(2);
            
            // Search through the index
            let result = index.query(
                query,
                &mut bm25::new(),
                tokenizer,
                &[1., 1.],
            );
            println!("Search results:");
            for (i, res) in result.iter().enumerate() {
                println!("{}. Score: {}", i + 1, res.score);
            }
        }
        _ => unreachable!()
    }

    Ok(())
}

async fn fetch_page(client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>, uri: Uri) -> Result<String, Box<dyn Error + Send + Sync>> {
    let req = Request::builder()
        .uri(uri)
        .body(Empty::new())?;

    // Send the request and get the response
    let res = client.request(req).await?;
    
    // Get the response body and convert to string
    let body = res.collect().await?.to_bytes();
    let content = String::from_utf8(body.to_vec())?;
    Ok(content)
}