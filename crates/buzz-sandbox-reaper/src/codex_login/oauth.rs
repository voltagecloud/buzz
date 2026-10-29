//! The pinned Codex 0.159.3 refresh contract. Never follow redirects and
//! never retain or report an upstream response body on failure.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{redirect::Policy, Client};
use serde::Deserialize;
use serde_json::{from_slice, json};
#[cfg(test)]
use url::Url;
use zeroize::Zeroizing;

use super::{Error, Tokens};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const MAX_RESPONSE: usize = 65_536;

/// Fixed upstream transport for the pinned native refresh grant.
pub struct OAuth {
    client: Client,
    endpoint: String,
}

#[derive(Deserialize)]
struct Refreshed {
    access_token: String,
    id_token: Option<String>,
    refresh_token: Option<String>,
}

impl OAuth {
    /// Build the pinned endpoint client with bounded responses and no redirects.
    pub fn new() -> Result<Self, Error> {
        Self::endpoint(ENDPOINT)
    }

    fn endpoint(endpoint: &str) -> Result<Self, Error> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(Policy::none())
            .build()
            .map_err(|_| Error::Upstream)?;
        Ok(Self {
            client,
            endpoint: endpoint.to_string(),
        })
    }

    #[cfg(test)]
    pub(super) fn local(endpoint: &str) -> Result<Self, Error> {
        let url = Url::parse(endpoint).map_err(|_| Error::Upstream)?;
        if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") {
            return Err(Error::Upstream);
        }
        Self::endpoint(endpoint)
    }

    pub(super) async fn refresh(&self, previous: &Tokens) -> Result<Tokens, Error> {
        let response = self
            .client
            .post(&self.endpoint)
            .json(&json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": previous.refresh_token,
            }))
            .send()
            .await
            .map_err(|_| Error::Upstream)?;
        if !response.status().is_success() {
            return Err(if response.status().is_client_error() {
                Error::Rejected
            } else {
                Error::Upstream
            });
        }
        let mut body = Zeroizing::new(Vec::new());
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| Error::Upstream)?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Error::Upstream);
            }
            body.extend_from_slice(&chunk);
        }
        let refreshed: Refreshed = from_slice(&body).map_err(|_| Error::Upstream)?;
        let tokens = Tokens {
            access_token: refreshed.access_token,
            id_token: refreshed
                .id_token
                .unwrap_or_else(|| previous.id_token.clone()),
            refresh_token: refreshed
                .refresh_token
                .unwrap_or_else(|| previous.refresh_token.clone()),
            account_id: previous.account_id.clone(),
        };
        tokens.validate().map_err(|_| Error::Upstream)?;
        Ok(tokens)
    }
}
