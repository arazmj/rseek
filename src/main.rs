use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use hyper::{Request, Uri};
use http_body_util::{BodyExt, Empty};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // Create a new HTTP client
    let client: Client<HttpConnector, Empty<bytes::Bytes>> = Client::builder(TokioExecutor::new())
        .build(HttpConnector::new());

    // Create a request to example.com
    let uri = "http://example.com".parse::<Uri>()?;
    let req = Request::builder()
        .uri(uri)
        .body(Empty::new())?;

    // Send the request and get the response
    let res = client.request(req).await?;
    
    // Get the response body
    let body = res.collect().await?.to_bytes();

    // Convert the body to a string and print it
    let content = String::from_utf8(body.to_vec())?;
    println!("Response from example.com:\n{}", content);

    Ok(())
}