//! GPUI's HTTP client, which its own image loader uses for `img(uri)`.
//!
//! The platform default on Linux cannot fetch at all, so a profile picture
//! handed to `img()` as a plain `https://` URL never loaded. This serves
//! those requests with reqwest on the tokio runtime. Relay media still goes
//! through the community's signed media client (`ui::media`); this client
//! sends no credentials and only `GET`s `https://` URLs.

use futures::future::BoxFuture;
use gpui_kit::http_client::http::{HeaderValue, Method, StatusCode};
use gpui_kit::http_client::{AsyncBody, HttpClient, Request, Response, Url};
use std::time::Duration;

/// Largest body served; only images are fetched this way.
const MAX_BODY: usize = 25 * 1024 * 1024;

pub struct Http {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

impl Http {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            // Redirects too, not just the first URL.
            .https_only(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("build the HTTP client");
        Self { client, runtime }
    }
}

impl HttpClient for Http {
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let (client, runtime) = (self.client.clone(), self.runtime.clone());
        let (parts, _) = request.into_parts();
        Box::pin(async move {
            let url = parts.uri.to_string();
            anyhow::ensure!(parts.method == Method::GET, "only GET is supported");
            anyhow::ensure!(
                url.starts_with("https://"),
                "only https:// URLs are fetched"
            );
            let (status, body) = runtime
                .spawn(async move {
                    let mut response = client.get(&url).send().await?;
                    let status = response.status().as_u16();
                    let mut body = Vec::new();
                    while let Some(chunk) = response.chunk().await? {
                        anyhow::ensure!(body.len() + chunk.len() <= MAX_BODY, "response too large");
                        body.extend_from_slice(&chunk);
                    }
                    anyhow::Ok((status, body))
                })
                .await??;
            Ok(Response::builder()
                .status(StatusCode::from_u16(status)?)
                .body(AsyncBody::from(body))?)
        })
    }
}
