use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use hyper::{Request, Uri};
use http_body_util::{BodyExt, Empty};
use html_parser::{Dom, Node};
use std::error::Error;
use bytes::Bytes;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Create a new HTTP client
    let client: Client<HttpConnector, Empty<bytes::Bytes>> = Client::builder(TokioExecutor::new())
        .build(HttpConnector::new());

    // Create a request to example.com
    let uri = "http://example.com".parse::<Uri>()?;

    let content = fetch_page(client, uri).await?;

    let hrefs = parse_links(&content)?;

    // Print all found URLs
    println!("Found URLs:");
    for (index, href) in hrefs.into_iter().enumerate() {
        println!("{}: {}", index + 1, href);
    }

    Ok(())
}

fn parse_links(content: &String) -> Result<Vec<String>, Box<dyn Error>> {
    // Parse the HTML content
    let dom = Dom::parse(&content)?;

    // Extract all URLs using iterator
    let iter = dom.children.get(0).unwrap().into_iter();
    let hrefs: Vec<_> = iter.filter_map(|item| match item {
        Node::Element(ref element) if element.name == "a" => {
            element.attributes.get("href").and_then(|h| h.clone())
        }
        _ => None,
    }).collect();
    Ok(hrefs)
}

async fn fetch_page(client: Client<HttpConnector, Empty<Bytes>>, uri: Uri) -> Result<String, Box<dyn Error>> {
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