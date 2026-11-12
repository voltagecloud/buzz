//! Presentation-only profile lookup, bounded independently of prompt handling.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use nostr::{Event, PublicKey};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

use super::bridge::BridgeRelay;

const CACHE_CAP: usize = 128;
const BATCH_CAP: usize = 32;
const TTL: Duration = Duration::from_secs(300);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Names {
    cache: Mutex<HashMap<PublicKey, (Instant, String)>>,
    queries: Semaphore,
}

impl Default for Names {
    fn default() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            queries: Semaphore::new(4),
        }
    }
}

pub(super) fn fallback(key: PublicKey) -> String {
    format!("{}…", &key.to_hex()[..8])
}

fn label(event: &Event) -> String {
    let profile: Value = serde_json::from_str(&event.content).unwrap_or_default();
    let name = ["display_name", "name"]
        .into_iter()
        .filter_map(|field| profile[field].as_str())
        .map(|name| {
            name.chars()
                .filter(|ch| !ch.is_control() && *ch != '[' && *ch != ']')
                .take(64)
                .collect::<String>()
        })
        .find(|name| !name.trim().is_empty())
        .unwrap_or_else(|| fallback(event.pubkey));
    // The NIP-OA profile marker is presentation here, never an authority grant.
    let agent = event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.len() == 4 && parts[0] == "auth"
    });
    if agent {
        format!("{name} (agent)")
    } else {
        name
    }
}

impl Names {
    /// Read an unexpired label without waiting for a lookup or the cache lock.
    pub(super) fn cached(&self, author: PublicKey) -> Option<String> {
        let cache = self.cache.try_lock().ok()?;
        cache
            .get(&author)
            .filter(|(inserted, _)| inserted.elapsed() < TTL)
            .map(|(_, name)| name.clone())
    }

    /// At most 128 authors are looked up per replay; other labels retain the
    /// key-prefix fallback. Cache misses never block the bridge actor.
    pub(super) async fn resolve<R: BridgeRelay>(
        &self,
        relay: &R,
        authors: impl IntoIterator<Item = PublicKey>,
    ) -> HashMap<PublicKey, String> {
        let authors: Vec<_> = authors
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .take(CACHE_CAP)
            .collect();
        let now = Instant::now();
        let mut labels = HashMap::new();
        {
            let mut cache = self.cache.lock().await;
            cache.retain(|_, (inserted, _)| now.duration_since(*inserted) < TTL);
            for author in &authors {
                if let Some((_, name)) = cache.get(author) {
                    labels.insert(*author, name.clone());
                }
            }
        }
        let missing: Vec<_> = authors
            .into_iter()
            .filter(|author| !labels.contains_key(author))
            .collect();
        for batch in missing.chunks(BATCH_CAP) {
            let lookup = async {
                let _permit = self.queries.acquire().await.ok()?;
                relay.query(json!({"kinds":[0],"authors":batch.iter().map(PublicKey::to_hex).collect::<Vec<_>>(),"limit":BATCH_CAP})).await.ok()
            };
            // A failed or timed-out lookup is not an answer: label this
            // replay with the key prefix, but cache nothing, so the next
            // message tries again.
            let Some(events) = tokio::time::timeout(LOOKUP_TIMEOUT, lookup)
                .await
                .ok()
                .flatten()
            else {
                for author in batch {
                    labels.insert(*author, fallback(*author));
                }
                continue;
            };
            let mut valid = events
                .into_iter()
                .take(BATCH_CAP)
                .filter(|event| {
                    event.kind.as_u16() == 0
                        && batch.contains(&event.pubkey)
                        && event.content.len() <= 64 * 1024
                        && event.verify().is_ok()
                })
                .collect::<Vec<_>>();
            valid.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let mut cache = self.cache.lock().await;
            for author in batch {
                let name = valid
                    .iter()
                    .find(|event| event.pubkey == *author)
                    .map(label)
                    .unwrap_or_else(|| fallback(*author));
                if cache.len() >= CACHE_CAP {
                    if let Some(oldest) = cache
                        .iter()
                        .min_by_key(|(_, (inserted, _))| *inserted)
                        .map(|(key, _)| *key)
                    {
                        cache.remove(&oldest);
                    }
                }
                cache.insert(*author, (Instant::now(), name.clone()));
                labels.insert(*author, name);
            }
        }
        labels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CliError;
    use nostr::{EventBuilder, Keys, Kind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Relay {
        profiles: Vec<Event>,
        queries: AtomicUsize,
        failing: std::sync::atomic::AtomicBool,
    }
    impl BridgeRelay for Relay {
        async fn query(&self, filter: Value) -> Result<Vec<Event>, CliError> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            if self.failing.load(Ordering::SeqCst) {
                return Err(CliError::Other("relay unavailable".into()));
            }
            Ok(self
                .profiles
                .iter()
                .filter(|event| {
                    filter["authors"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(event.pubkey.to_hex()))
                })
                .cloned()
                .collect())
        }
        async fn prepare_prompt(&self, _: String, _: Option<String>) -> Result<Event, CliError> {
            Err(CliError::Other("unused".into()))
        }
        async fn submit(&self, _: Event) -> Result<(), CliError> {
            Err(CliError::Other("unused".into()))
        }
        async fn publish_ephemeral(&self, _: Event) -> Result<(), CliError> {
            Err(CliError::Other("unused".into()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn profile_cache_expires_bounds_entries_and_rejects_forged_names() {
        let author = Keys::generate();
        let profile = EventBuilder::new(
            Kind::from(0),
            json!({"display_name":format!("[{}]\\n", "x".repeat(100))}).to_string(),
        )
        .sign_with_keys(&author)
        .unwrap();
        let forged_author = Keys::generate();
        let mut forged = EventBuilder::new(Kind::from(0), json!({"name":"forged"}).to_string())
            .sign_with_keys(&forged_author)
            .unwrap();
        forged.content = json!({"name":"injected forged name"}).to_string();
        let relay = Relay {
            profiles: vec![profile, forged],
            queries: AtomicUsize::new(0),
            failing: std::sync::atomic::AtomicBool::new(false),
        };
        let names = Names::default();
        for _ in 0..2 {
            let labels = names
                .resolve(&relay, [author.public_key(), forged_author.public_key()])
                .await;
            assert_eq!(labels[&author.public_key()], "x".repeat(64));
            assert_eq!(
                labels[&forged_author.public_key()],
                fallback(forged_author.public_key())
            );
        }
        assert_eq!(relay.queries.load(Ordering::SeqCst), 1);
        assert_eq!(names.cached(author.public_key()), Some("x".repeat(64)));
        {
            let _locked = names.cache.lock().await;
            assert!(names.cached(author.public_key()).is_none());
        }
        tokio::time::advance(TTL + Duration::from_secs(1)).await;
        assert!(names.cached(author.public_key()).is_none());
        // A failed lookup labels with the key prefix but caches nothing, so
        // the next message asks again and gets the real name.
        relay.failing.store(true, Ordering::SeqCst);
        let labels = names.resolve(&relay, [author.public_key()]).await;
        assert_eq!(labels[&author.public_key()], fallback(author.public_key()));
        assert!(names.cached(author.public_key()).is_none());
        relay.failing.store(false, Ordering::SeqCst);
        names.resolve(&relay, [author.public_key()]).await;
        assert_eq!(relay.queries.load(Ordering::SeqCst), 3);
        assert_eq!(names.cached(author.public_key()), Some("x".repeat(64)));
        for _ in 0..2 {
            let authors = (0..CACHE_CAP)
                .map(|_| Keys::generate().public_key())
                .collect::<Vec<_>>();
            names.resolve(&relay, authors).await;
        }
        assert_eq!(names.cache.lock().await.len(), CACHE_CAP);
    }
}
