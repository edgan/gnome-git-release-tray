use crate::model::Release;
use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode, header};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// GitHub lists at most the newest 1000 releases (10 pages of 100) and answers
/// unauthenticated requests for later pages with 422.
const MAX_PAGES: u32 = 10;

pub struct GitHub {
    client: Client,
    base: String,
    retry_at: Option<Instant>,
}

impl GitHub {
    pub fn new() -> Result<Self> {
        Self::with_base(
            "https://api.github.com".into(),
            std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty()),
        )
    }
    pub(crate) fn with_base(base: String, token: Option<String>) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            header::HeaderValue::from_static("2022-11-28"),
        );
        if let Some(token) = token {
            let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("GITHUB_TOKEN is not a valid HTTP header")?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }
        Ok(Self {
            client: Client::builder()
                .user_agent("gnome-git-release-tray/0.1.0")
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
            base,
            retry_at: None,
        })
    }
    /// Lists releases newest first: every page up to GitHub's cap when
    /// `all_pages` is set, otherwise only the newest page.
    pub async fn releases(&mut self, repository: &str, all_pages: bool) -> Result<Vec<Release>> {
        if let Some(until) = self.retry_at.filter(|t| *t > Instant::now()) {
            bail!(
                "GitHub rate limit: retry in {} seconds",
                until.duration_since(Instant::now()).as_secs() + 1
            );
        }
        let mut all = vec![];
        let pages = if all_pages { MAX_PAGES } else { 1 };
        for page in 1..=pages {
            let response = self
                .client
                .get(format!("{}/repos/{repository}/releases", self.base))
                .query(&[("per_page", 100), ("page", page)])
                .send()
                .await
                .context("Cannot contact GitHub; check your network connection")?;
            let status = response.status();
            if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
                let headers = response.headers();
                let retry = headers
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                let reset = headers
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let delay = retry
                    .or_else(|| reset.map(|t| t.saturating_sub(now) + 1))
                    .unwrap_or(60)
                    .clamp(1, 86400);
                self.retry_at = Some(Instant::now() + Duration::from_secs(delay));
                bail!(
                    "GitHub denied or rate-limited the request ({status}); retry in {delay} seconds. Check token permissions or API quota."
                );
            }
            if status == StatusCode::NOT_FOUND {
                bail!("Repository not found; check the URL and that the repository is public");
            }
            if status == StatusCode::UNAUTHORIZED {
                bail!("GitHub rejected GITHUB_TOKEN; update or remove it and restart");
            }
            if !status.is_success() {
                bail!("GitHub returned {status}; try again later");
            }
            let has_next = response
                .headers()
                .get(header::LINK)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| s.split(',').any(|link| link.contains("rel=\"next\"")));
            let batch: Vec<Release> = response
                .json()
                .await
                .context("GitHub returned invalid release data")?;
            all.extend(batch);
            if !has_next {
                break;
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    pub(crate) fn server(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let join = thread::spawn(move || {
            responses
                .into_iter()
                .map(|response| {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = vec![];
                    let mut buf = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut buf).unwrap();
                        if n == 0 {
                            break;
                        }
                        request.extend_from_slice(&buf[..n]);
                    }
                    stream.write_all(response.as_bytes()).unwrap();
                    String::from_utf8(request).unwrap()
                })
                .collect()
        });
        (base, join)
    }
    pub(crate) fn response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }
    #[test]
    fn paginates_and_sends_headers() {
        let (base, join) = server(vec![
            response("200 OK", "Link: <http://unused>; rel=\"next\"\r\n", "[]"),
            response("200 OK", "", "[]"),
        ]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut client = GitHub::with_base(base, Some("test-token".into())).unwrap();
        assert!(
            rt.block_on(client.releases("a/b", true))
                .unwrap()
                .is_empty()
        );
        let requests = join.join().unwrap();
        assert!(requests[0].contains("per_page=100&page=1"));
        assert!(requests[1].contains("per_page=100&page=2"));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("authorization: bearer test-token")
        );
    }
    #[test]
    fn stops_at_github_page_cap() {
        let next = "Link: <http://unused>; rel=\"next\"\r\n";
        let (base, join) = server(vec![response("200 OK", next, "[]"); 11]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut client = GitHub::with_base(base, None).unwrap();
        assert!(
            rt.block_on(client.releases("a/b", true))
                .unwrap()
                .is_empty()
        );
        assert!(
            rt.block_on(client.releases("a/b", false))
                .unwrap()
                .is_empty()
        );
        let requests = join.join().unwrap();
        assert!(requests[9].contains("per_page=100&page=10"));
        assert!(requests[10].contains("per_page=100&page=1 "));
    }
    #[test]
    fn rate_limit_prevents_repeat_requests() {
        let (base, join) = server(vec![response(
            "429 Too Many Requests",
            "Retry-After: 120\r\n",
            "{}",
        )]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut client = GitHub::with_base(base, None).unwrap();
        assert!(
            rt.block_on(client.releases("a/b", true))
                .unwrap_err()
                .to_string()
                .contains("120")
        );
        assert!(
            rt.block_on(client.releases("a/b", true))
                .unwrap_err()
                .to_string()
                .contains("rate limit")
        );
        join.join().unwrap();
    }
}
