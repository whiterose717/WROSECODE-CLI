use anyhow::{bail, Result};
use reqwest::Client;

pub async fn request(
    client: &Client,
    method: &str,
    url: &str,
    body: Option<&str>,
) -> Result<String> {
    let method = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.body(body.to_owned());
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    Ok(format!("HTTP {status}\n{text}"))
}

pub async fn web_search(client: &Client, query: &str) -> Result<String> {
    let endpoint = std::env::var("WROSECODE_SEARCH_URL")
        .unwrap_or_else(|_| "https://html.duckduckgo.com/html/".into());
    let response = client.get(endpoint).query(&[("q", query)]).send().await?;
    if !response.status().is_success() {
        bail!("search HTTP {}", response.status());
    }
    Ok(response.text().await?)
}
