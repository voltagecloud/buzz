//! GIF search for one session (`gifs.ts`). Only HTTPS communities offer
//! it, as in the reference (`communityFromScope`).

use super::Session;
use crate::gifs::{self, Gif, SEARCH_PATH, UNAVAILABLE};
use crate::http::HttpError;
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// Communities whose NIP-11 advertised GIF search (`nativeSearchPaths`).
/// Only support is kept, since a relay can enable GIFs while the app runs;
/// a failed discovery or search is forgotten so a change is rediscovered.
static SUPPORTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn supported(community: &str) -> bool {
    SUPPORTED.lock().expect("gif cache").contains(community)
}

fn remember(community: &str, available: bool) {
    let mut cache = SUPPORTED.lock().expect("gif cache");
    if available {
        cache.insert(community.to_owned());
    } else {
        cache.remove(community);
    }
}

impl Session {
    /// Whether the relay advertises KLIPY search in NIP-11, remembered
    /// per community once it does.
    pub async fn gif_search_available(&self) -> bool {
        let community = self.origin().http();
        if !community.starts_with("https://") {
            return false;
        }
        if supported(&community) {
            return true;
        }
        let http = self.direct.http.clone();
        let available = self
            .run(async move {
                let info = http
                    .information()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(gifs::search_path(&info).is_some())
            })
            .await
            .unwrap_or(false);
        remember(&community, available);
        available
    }

    /// One page of GIFs for `query`, trending when it is empty. The
    /// relay forwards it to the provider with its own key.
    pub async fn search_gifs(
        &self,
        query: &str,
        customer_id: &str,
        locale: &str,
    ) -> Result<Vec<Gif>, String> {
        let community = self.origin().http();
        if !community.starts_with("https://") || !self.gif_search_available().await {
            return Err(UNAVAILABLE.into());
        }
        let http = self.direct.http.clone();
        let body = gifs::search_body(customer_id, locale, query);
        let result = self
            .run(async move {
                let bytes = http
                    .post(SEARCH_PATH, body)
                    .await
                    .map_err(|error| match error {
                        HttpError::Status {
                            status, message, ..
                        } if message.trim().is_empty() => format!("GIF request failed ({status})"),
                        HttpError::Status { message, .. } => message,
                        other => other.to_string(),
                    })?;
                let response: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|_| "GIF search failed".to_string())?;
                gifs::parse(&response)
            })
            .await;
        // A failed search may mean the relay disabled GIFs since discovery.
        if result.is_err() {
            remember(&community, false);
        }
        result
    }
}
