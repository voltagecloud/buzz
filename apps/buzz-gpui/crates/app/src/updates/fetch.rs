//! Asking GitHub for the latest release, and downloading its files.
//!
//! The API is used without a token, so its limit is 60 requests an hour
//! per address. Each check sends the last response's ETag; an unchanged
//! release answers 304, which costs nothing against the limit. A limited
//! response sets a time before which no request is sent at all.

use super::release::Release;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A check gives up after this long.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// A download gives up after this long.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The largest package downloaded.
pub const MAX_PACKAGE: u64 = 1024 * 1024 * 1024;
/// The largest `SHA256SUMS` read.
pub const MAX_CHECKSUMS: u64 = 64 * 1024;
/// How long a limited answer with no reset time holds checks off, as
/// GitHub asks for secondary limits.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);

/// What the last check learned, kept for the next.
#[derive(Debug, Default)]
pub struct Cache {
    etag: Option<String>,
    release: Option<Release>,
    /// No request goes out before this.
    blocked_until: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// GitHub limited this address; checks wait until then.
    RateLimited(SystemTime),
    Failed(String),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(until) => {
                let local: chrono::DateTime<chrono::Local> = (*until).into();
                write!(
                    f,
                    "GitHub is limiting update checks. Try again after {}.",
                    local.format("%-I:%M %p")
                )
            }
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// The response headers that steer caching and limits.
#[derive(Debug, Default)]
pub struct Headers {
    pub etag: Option<String>,
    pub retry_after: Option<u64>,
    pub remaining: Option<u64>,
    pub reset: Option<u64>,
}

impl Headers {
    fn of(headers: &reqwest::header::HeaderMap) -> Self {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let number = |name: &str| text(name).and_then(|value| value.trim().parse().ok());
        Self {
            etag: text("etag"),
            retry_after: number("retry-after"),
            remaining: number("x-ratelimit-remaining"),
            reset: number("x-ratelimit-reset"),
        }
    }

    /// Until when the answer asks us to wait, if it does.
    fn wait_until(&self, status: u16, now: SystemTime) -> Option<SystemTime> {
        let reset = self
            .reset
            .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
            .filter(|reset| *reset > now);
        if let Some(seconds) = self.retry_after {
            return Some(now + Duration::from_secs(seconds));
        }
        if self.remaining == Some(0) {
            return Some(reset.unwrap_or(now + DEFAULT_BACKOFF));
        }
        matches!(status, 403 | 429).then(|| now + DEFAULT_BACKOFF)
    }
}

impl Cache {
    /// Whether a request may go out at `now`.
    fn check_allowed(&self, now: SystemTime) -> Result<(), CheckError> {
        match self.blocked_until {
            Some(until) if until > now => Err(CheckError::RateLimited(until)),
            _ => Ok(()),
        }
    }

    /// Takes in one answer: the latest release, `None` when the repo has
    /// published none.
    pub fn answer(
        &mut self,
        status: u16,
        headers: &Headers,
        body: &[u8],
        now: SystemTime,
    ) -> Result<Option<Release>, CheckError> {
        let wait = headers.wait_until(status, now);
        self.blocked_until = wait;
        match status {
            200 => {
                let release: Release = serde_json::from_slice(body).map_err(|error| {
                    CheckError::Failed(format!("GitHub sent a release this can't read: {error}"))
                })?;
                self.etag = headers.etag.clone();
                self.release = Some(release.clone());
                Ok(Some(release))
            }
            304 => match &self.release {
                Some(release) => Ok(Some(release.clone())),
                // Nothing to reuse; drop the tag so the next check asks
                // again in full.
                None => {
                    self.etag = None;
                    Err(CheckError::Failed("GitHub sent nothing new".into()))
                }
            },
            // No published release yet.
            404 => {
                self.etag = None;
                self.release = None;
                Ok(None)
            }
            403 | 429 if wait.is_some() => Err(CheckError::RateLimited(wait.unwrap_or(now))),
            status => Err(CheckError::Failed(format!(
                "GitHub answered the update check with HTTP {status}"
            ))),
        }
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("buzz-gpui/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        // Redirects too, as `allowed_url` asks of the first URL.
        .https_only(!cfg!(debug_assertions))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .expect("build the update client")
}

/// Asks `url` for the latest release.
pub async fn latest(
    client: &reqwest::Client,
    url: &str,
    cache: &Mutex<Cache>,
) -> Result<Option<Release>, CheckError> {
    let etag = {
        let cache = cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.check_allowed(SystemTime::now())?;
        cache.etag.clone()
    };
    let mut request = client
        .get(url)
        .timeout(CHECK_TIMEOUT)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28");
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag);
    }
    let response = request
        .send()
        .await
        .map_err(|error| CheckError::Failed(format!("Couldn't reach GitHub: {error}")))?;
    let status = response.status().as_u16();
    let headers = Headers::of(response.headers());
    let body = read_limited(response, 4 * 1024 * 1024)
        .await
        .map_err(CheckError::Failed)?;
    cache.lock().unwrap_or_else(|p| p.into_inner()).answer(
        status,
        &headers,
        &body,
        SystemTime::now(),
    )
}

/// A small text file, such as `SHA256SUMS`.
pub async fn text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = get(client, url, CHECK_TIMEOUT).await?;
    let body = read_limited(response, MAX_CHECKSUMS).await?;
    String::from_utf8(body).map_err(|_| "The checksum list isn't text".to_string())
}

/// Downloads `url` to `dest`, by way of a partial file beside it, counting
/// bytes into `progress`.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    progress: &AtomicU64,
) -> Result<(), String> {
    let mut response = get(client, url, DOWNLOAD_TIMEOUT).await?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PACKAGE)
    {
        return Err("The update is too large".into());
    }
    let partial = dest.with_extension("partial");
    let write_error = |error: std::io::Error| format!("Couldn't save the update: {error}");
    let mut file = std::fs::File::create(&partial).map_err(write_error)?;
    let mut written = 0u64;
    progress.store(0, Ordering::Relaxed);
    let result = async {
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("The download stopped: {error}"))?
        {
            written += chunk.len() as u64;
            if written > MAX_PACKAGE {
                return Err("The update is too large".to_string());
            }
            file.write_all(&chunk).map_err(write_error)?;
            progress.store(written, Ordering::Relaxed);
        }
        file.sync_all().map_err(write_error)
    }
    .await;
    drop(file);
    match result {
        Ok(()) => std::fs::rename(&partial, dest).map_err(write_error),
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
}

async fn get(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<reqwest::Response, String> {
    if !super::allowed_url(url) {
        return Err(format!("Refusing to download from {url}"));
    }
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| format!("Couldn't download the update: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("The download failed with HTTP {}", status.as_u16()));
    }
    Ok(response)
}

async fn read_limited(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("The response stopped: {error}"))?
    {
        if (body.len() + chunk.len()) as u64 > limit {
            return Err("The response is too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FIXTURE: &[u8] = include_bytes!("fixtures/release.json");

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn an_unchanged_release_is_reused() {
        let mut cache = Cache::default();
        let headers = Headers {
            etag: Some("\"abc\"".into()),
            remaining: Some(59),
            ..Headers::default()
        };
        let first = cache.answer(200, &headers, FIXTURE, at(100)).unwrap();
        assert_eq!(cache.etag.as_deref(), Some("\"abc\""));
        let again = cache
            .answer(304, &Headers::default(), b"", at(200))
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(first.unwrap().tag_name, "v0.1.1");
    }

    #[test]
    fn not_modified_without_a_cached_release_asks_again() {
        let mut cache = Cache {
            etag: Some("\"stale\"".into()),
            ..Cache::default()
        };
        assert!(cache.answer(304, &Headers::default(), b"", at(1)).is_err());
        assert_eq!(cache.etag, None);
    }

    #[test]
    fn no_release_is_not_an_error() {
        let mut cache = Cache::default();
        assert_eq!(
            cache.answer(404, &Headers::default(), b"{}", at(1)),
            Ok(None)
        );
    }

    #[test]
    fn a_spent_limit_waits_for_its_reset() {
        let mut cache = Cache::default();
        let headers = Headers {
            remaining: Some(0),
            reset: Some(5_000),
            ..Headers::default()
        };
        assert_eq!(
            cache.answer(403, &headers, b"{}", at(1_000)),
            Err(CheckError::RateLimited(at(5_000)))
        );
        assert_eq!(
            cache.check_allowed(at(4_999)),
            Err(CheckError::RateLimited(at(5_000)))
        );
        assert_eq!(cache.check_allowed(at(5_000)), Ok(()));
    }

    #[test]
    fn retry_after_wins_and_bare_limits_back_off() {
        let mut cache = Cache::default();
        let headers = Headers {
            retry_after: Some(120),
            ..Headers::default()
        };
        assert_eq!(
            cache.answer(429, &headers, b"", at(1_000)),
            Err(CheckError::RateLimited(at(1_120)))
        );
        assert_eq!(
            cache.answer(403, &Headers::default(), b"", at(2_000)),
            Err(CheckError::RateLimited(at(2_060)))
        );
        // The last request of the hour succeeds, and holds the next off.
        let headers = Headers {
            remaining: Some(0),
            reset: Some(9_000),
            ..Headers::default()
        };
        assert!(cache.answer(200, &headers, FIXTURE, at(3_000)).is_ok());
        assert!(cache.check_allowed(at(8_000)).is_err());
    }

    #[test]
    fn other_failures_say_so() {
        let mut cache = Cache::default();
        let error = cache
            .answer(500, &Headers::default(), b"", at(1))
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 500"));
        assert!(
            cache
                .answer(200, &Headers::default(), b"<html>", at(1))
                .is_err()
        );
    }

    /// Serves canned answers on a local port, recording each request's
    /// head.
    async fn serve(answers: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut heads = Vec::new();
            for answer in answers {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                heads.push(String::from_utf8(head).unwrap().to_lowercase());
                stream.write_all(&answer).await.unwrap();
                stream.shutdown().await.unwrap();
            }
            heads
        });
        (base, handle)
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[tokio::test]
    async fn checks_with_the_etag_and_downloads() {
        let package = b"package bytes".repeat(1000);
        let (base, server) = serve(vec![
            response(
                "200 OK",
                "etag: \"v1\"\r\nx-ratelimit-remaining: 58\r\n",
                FIXTURE,
            ),
            response("304 Not Modified", "etag: \"v1\"\r\n", b""),
            response("200 OK", "", &package),
        ])
        .await;
        let client = client();
        let cache = Mutex::new(Cache::default());
        let url = format!("{base}/repos/o/r/releases/latest");
        let first = latest(&client, &url, &cache).await.unwrap().unwrap();
        let second = latest(&client, &url, &cache).await.unwrap().unwrap();
        assert_eq!(first, second);

        let dir = std::env::temp_dir().join(format!("buzz-update-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("package.deb");
        let progress = AtomicU64::new(0);
        download(&client, &format!("{base}/package.deb"), &dest, &progress)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), package);
        assert_eq!(progress.load(Ordering::Relaxed), package.len() as u64);
        assert!(!dest.with_extension("partial").exists());
        std::fs::remove_dir_all(&dir).unwrap();

        let heads = server.await.unwrap();
        assert!(heads[0].contains("accept: application/vnd.github+json"));
        assert!(heads[0].contains("user-agent: buzz-gpui/"));
        assert!(!heads[0].contains("if-none-match"));
        assert!(heads[1].contains("if-none-match: \"v1\""));
    }
}
