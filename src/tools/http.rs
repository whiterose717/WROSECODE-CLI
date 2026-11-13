use anyhow::{bail, Context, Result};
use regex::Regex;
use reqwest::Client;
use std::sync::OnceLock;
use std::time::Duration;

/// Tool HTTP shares the unbounded streaming client, so every call carries
/// its own total bound here instead: a stalled page fetch must fail as a
/// tool error, never hang the turn.
async fn bounded<T>(fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    bounded_secs(120, fut).await
}

async fn bounded_secs<T>(
    secs: u64,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .context("tool HTTP timed out")?
}

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
    bounded(async move {
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        Ok(format!("HTTP {status}\n{text}"))
    })
    .await
}

/// Bug-bounty HTTP requests are deliberately bounded and never follow
/// redirects automatically. A 302 from an in-scope asset to an unapproved
/// third-party host must not cause the agent to contact that third party.
/// The response is truncated at 1 MiB to keep long audit sessions responsive.
pub async fn request_bounty(method: &str, url: &str, body: Option<&str>) -> Result<String> {
    const MAX_BODY_BYTES: usize = 1024 * 1024;
    let method = reqwest::Method::from_bytes(method.as_bytes())?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(25))
        .build()?;
    bounded_secs(25, async move {
        let mut request = client.request(method, url);
        if let Some(body) = body {
            request = request.body(body.to_owned());
        }
        let mut response = request.send().await?;
        let status = response.status();
        let mut bytes = Vec::new();
        let mut clipped = false;
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                let remaining = MAX_BODY_BYTES.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..remaining]);
                clipped = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut result = format!("HTTP {status}\n{}", String::from_utf8_lossy(&bytes));
        if clipped {
            result.push_str("\n[response truncated at 1 MiB to prevent oversized evidence]");
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            result.push_str("\n[rate limited: stop probing and respect the program policy]");
        } else if status.is_redirection() {
            result.push_str(
                "\n[redirect not followed: validate the destination against program scope]",
            );
        }
        Ok(result)
    })
    .await
}

pub async fn web_search(client: &Client, query: &str) -> Result<String> {
    let endpoint = std::env::var("WROSECODE_SEARCH_URL")
        .unwrap_or_else(|_| "https://html.duckduckgo.com/html/".into());
    bounded(async move {
        let response = client.get(endpoint).query(&[("q", query)]).send().await?;
        if !response.status().is_success() {
            bail!("search HTTP {}", response.status());
        }
        Ok(search_text(response.text().await?))
    })
    .await
}

/// Search backends return a full HTML page. Markup is noise for the model and
/// gives hidden text a free ride into the context window; keep the readable
/// text, drop scripts/styles/tags, and decode the common entities.
fn search_text(body: String) -> String {
    if !body.trim_start().starts_with('<') {
        return body;
    }
    static SCRIPTS: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static ENTITIES: OnceLock<Regex> = OnceLock::new();
    let scripts = SCRIPTS.get_or_init(|| {
        Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>|<!--.*?-->")
            .expect("valid script pattern")
    });
    let tags = TAGS.get_or_init(|| Regex::new(r"(?s)<[^>]*>").expect("valid tag pattern"));
    let entities = ENTITIES.get_or_init(|| {
        Regex::new(r"&(?:#(\d{1,7})|#x([0-9a-fA-F]{1,6})|([a-zA-Z]{2,8}));")
            .expect("valid entity pattern")
    });
    let without_scripts = scripts.replace_all(&body, " ");
    let without_tags = tags.replace_all(&without_scripts, " ");
    let decoded = entities.replace_all(&without_tags, |caps: &regex::Captures| {
        let numeric = caps
            .get(1)
            .and_then(|value| value.as_str().parse::<u32>().ok())
            .or_else(|| {
                caps.get(2)
                    .and_then(|value| u32::from_str_radix(value.as_str(), 16).ok())
            })
            .and_then(char::from_u32);
        let named = match caps.get(3).map(|value| value.as_str()) {
            Some("amp") => Some('&'),
            Some("lt") => Some('<'),
            Some("gt") => Some('>'),
            Some("quot") => Some('"'),
            Some("apos") | Some("nbsp") => Some(' '),
            _ => None,
        };
        numeric
            .or(named)
            .map(|ch| ch.to_string())
            .unwrap_or_else(|| caps[0].to_string())
    });
    decoded
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stalled tool server must fail fast with a timeout, never hang the
    /// turn: accept the connection, then go silent.
    #[tokio::test]
    async fn stalled_tool_http_fails_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stall", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            use tokio::io::AsyncReadExt;
            let _ = socket.read(&mut buf).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let client = Client::new();
        let started = std::time::Instant::now();
        let outcome = bounded_secs(1, async move {
            let response = client.get(&url).send().await?;
            Ok(response.text().await?)
        })
        .await;
        let elapsed = started.elapsed();
        assert!(outcome.is_err(), "a stall must fail, not hang");
        assert!(
            elapsed < Duration::from_secs(30),
            "bounded quickly, not after a minute: {elapsed:?}"
        );
        server.abort();
    }

    #[test]
    fn search_html_is_reduced_to_readable_text() {
        let body = "<!doctype html><html><head><style>.x{color:#fff}</style>\
<script>alert('inject')</script></head><body>\
<h2>Results</h2><p>Hello &amp; welcome</p><a href=\"/x\">link</a>\
<!-- hidden --></body></html>";
        let text = search_text(body.into());
        assert!(text.contains("Hello & welcome"), "{text}");
        assert!(text.contains("Results"));
        assert!(!text.contains("alert"), "{text}");
        assert!(!text.contains("<h2>"), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn search_text_leaves_plain_payloads_alone() {
        assert_eq!(search_text("hello world".into()), "hello world");
        assert_eq!(
            search_text("{\"results\":[{\"title\":\"x\"}]}".into()),
            "{\"results\":[{\"title\":\"x\"}]}"
        );
    }

    #[tokio::test]
    async fn bounty_redirect_does_not_contact_a_second_host_or_port() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let outside = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let outside_port = outside.local_addr().unwrap().port();
        let inside = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let initial_port = inside.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = inside.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{outside_port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let result = request_bounty("GET", &format!("http://127.0.0.1:{initial_port}/"), None)
            .await
            .unwrap();
        assert!(result.contains("HTTP 302"), "{result}");
        assert!(result.contains("redirect not followed"), "{result}");
        assert!(
            tokio::time::timeout(Duration::from_millis(450), outside.accept())
                .await
                .is_err(),
            "redirect contacted a different port"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bounty_http_429_is_not_automatically_retried() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 512];
            let _ = stream.read(&mut buffer).await;
            stream.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            // Any second request would be an unwanted retry.
            assert!(
                tokio::time::timeout(Duration::from_millis(400), listener.accept())
                    .await
                    .is_err()
            );
        });
        let output = request_bounty("GET", &format!("http://{addr}/"), None)
            .await
            .unwrap();
        assert!(output.contains("HTTP 429"), "{output}");
        assert!(output.contains("rate limited"), "{output}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn healthy_tool_http_passes_through() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ok", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            let _ = socket.read(&mut buf).await;
            let body = "hello";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = Client::new();
        let text = bounded_secs(10, async move {
            let response = client.get(&url).send().await?;
            Ok(response.text().await?)
        })
        .await
        .expect("healthy server answers");
        assert_eq!(text, "hello");
        server.abort();
    }
}
