//! NIP-98 signed HTTP bridge operations. Every query requests writer consistency.

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) struct Relay {
    pub keys: Keys,
    base: String,
    client: reqwest::Client,
}

impl Relay {
    pub fn new(base: &str, keys: Keys) -> Result<Self> {
        super::config::secure_url(base)?;
        Ok(Self {
            keys,
            base: base.trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    async fn post(&self, path: &str, value: Value) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let body = serde_json::to_vec(&value)?;
        let tags = [
            ("u", url),
            ("method", "POST".to_owned()),
            ("payload", hex::encode(Sha256::digest(&body))),
            ("nonce", uuid::Uuid::new_v4().to_string()),
        ]
        .into_iter()
        .map(|(name, value)| Tag::parse([name, &value]))
        .collect::<std::result::Result<Vec<_>, _>>()?;
        let auth = EventBuilder::new(Kind::Custom(27235), "")
            .tags(tags)
            .sign_with_keys(&self.keys)?;
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header(
                "Authorization",
                format!("Nostr {}", STANDARD.encode(auth.as_json())),
            )
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("relay request refused");
        }
        // Bound even a chunked response before parsing it.
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > 128 * 1024 {
                bail!("relay response exceeded bound");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("invalid relay response")
    }

    pub async fn query(&self, mut filter: Value) -> Result<Vec<Event>> {
        filter["consistency"] = json!("strong");
        let value = self.post("/query", json!([filter])).await?;
        let events: Vec<Event> =
            serde_json::from_value(value).context("invalid relay event array")?;
        if events.len() > 2 {
            bail!("relay exceeded exact-query event bound");
        }
        Ok(events)
    }

    pub async fn submit(&self, event: &Event) -> Result<()> {
        let response = self.post("/events", serde_json::to_value(event)?).await?;
        // /events returns ["OK", id, accepted, message], as buzz-cli normalizes.
        let accepted = response
            .get("accepted")
            .and_then(Value::as_bool)
            .or_else(|| {
                response
                    .as_array()
                    .and_then(|items| items.get(2))
                    .and_then(Value::as_bool)
            });
        let acknowledged = response
            .get("event_id")
            .or_else(|| response.get("id"))
            .and_then(Value::as_str)
            .or_else(|| {
                response
                    .as_array()
                    .and_then(|parts| parts.get(1))
                    .and_then(Value::as_str)
            });
        if accepted != Some(true) || acknowledged != Some(event.id.to_hex().as_str()) {
            bail!("relay did not accept event");
        }
        Ok(())
    }

    pub async fn ready(&self) -> Result<()> {
        self.query(json!({"kinds":[buzz_core::kind::KIND_INTEGRATION_RECORD],"authors":[self.keys.public_key().to_hex()],"limit":1})).await.map(drop)
    }
}
