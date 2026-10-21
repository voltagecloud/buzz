//! An agent's own community as its owner reads it, for agent types that
//! act in it: channels, conversations for context, names, and the agent's
//! memory, which is encrypted to the owner.

use crate::agents2::Agents2;
use crate::buzz_mcp::{BuzzEvent, Memory};
use crate::community::{AppState, Community, IdentityState};
use crate::runtime::Runtime;
use buzz_identity::Identity;
use buzz_relay_client::session::{MeKitStatus, SessionSnapshot};
use buzz_relay_client::{ChannelType, RelayHttp, Session};
use futures::future::BoxFuture;
use gpui_kit::{App, Entity, EntityId};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// A channel as the owner's session sees it.
#[derive(Clone, Debug, Default)]
pub struct ChannelInfo {
    pub name: Option<String>,
    pub dm: bool,
    pub members: Vec<String>,
}

/// What an agent type's runtime reads as the owner: the agent's own
/// community's session, selected or not.
pub trait OwnerRelay: Send + Sync {
    fn channel(&self, id: &str) -> Option<ChannelInfo>;
    /// Reads the community as the owner.
    fn read(&self, filters: Vec<Value>) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>>;
    /// Display names of `pubkeys`, waiting briefly for profiles not yet
    /// known.
    fn names(
        &self,
        pubkeys: Vec<String>,
        members: Vec<String>,
    ) -> BoxFuture<'static, HashMap<String, String>>;
    /// The agent's memory, read back with the owner's key: it is encrypted
    /// to the owner, so no agent key is needed to read it.
    fn memories(&self, agent: &str) -> BoxFuture<'static, Result<Vec<Memory>, String>>;
    /// [`Self::memories`], and whether some records could not be read,
    /// so a missing `core` is not taken for one the agent never wrote.
    fn memory_listing(
        &self,
        agent: &str,
    ) -> BoxFuture<'static, Result<(Vec<Memory>, bool), String>> {
        let read = self.memories(agent);
        Box::pin(async move { read.await.map(|memories| (memories, false)) })
    }
    /// The community's own signing key (NIP-11 `self`), which signs its
    /// workflows' messages, once known.
    fn authority(&self) -> Option<String> {
        None
    }
    /// The NIP-OA owner attested in `pubkey`'s newest signed profile, if
    /// it can be read. A failed read is no verdict.
    fn profile_owner(&self, _pubkey: &str) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }
    /// Whether a mention of an agent `owner` owns continues its Me chat in
    /// `channel` (buzz-app's `meChat`): one channel conversation that posts
    /// at the channel's top level. Only the owner's own placement counts,
    /// and DMs keep their routing. A placement that cannot be read is no
    /// answer: the turn fails rather than guess where to post.
    fn me_chat(&self, _owner: &str, _channel: &str) -> BoxFuture<'static, Result<bool, String>> {
        Box::pin(async { Ok(false) })
    }
}

/// A community, read with the owner's key: conversations for
/// context, names, and the agent's memory, which is encrypted to the owner.
pub struct AppRelay {
    pub(crate) http: RelayHttp,
    pub(crate) identity: Arc<Identity>,
    /// The session's view: channels and profiles. `None` reads only
    /// over HTTP.
    pub(crate) snapshot: Option<tokio::sync::watch::Receiver<Arc<SessionSnapshot>>>,
    /// The session, for its Me placement. `None` reads only over HTTP.
    pub(crate) session: Option<Weak<Session>>,
}

fn filters(values: Vec<Value>) -> Vec<buzz_relay_client::socket::Filter> {
    values
        .into_iter()
        .filter_map(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .collect()
}

fn profile_name(content: &str) -> Option<String> {
    let profile: Value = serde_json::from_str(content).ok()?;
    ["display_name", "name"]
        .iter()
        .filter_map(|key| profile.get(*key).and_then(Value::as_str))
        .find(|name| !name.trim().is_empty())
        .map(str::to_string)
}

impl OwnerRelay for AppRelay {
    fn channel(&self, id: &str) -> Option<ChannelInfo> {
        let snapshot = self.snapshot.as_ref()?.borrow().clone();
        snapshot
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .map(|channel| ChannelInfo {
                name: Some(channel.name.clone()),
                dm: channel.channel_type == Some(ChannelType::Dm),
                members: channel.members.clone(),
            })
    }

    fn read(&self, values: Vec<Value>) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>> {
        let http = self.http.clone();
        Box::pin(async move {
            let events = http
                .query(&filters(values))
                .await
                .map_err(|error| error.to_string())?;
            Ok(events
                .iter()
                .filter_map(|event| {
                    serde_json::to_value(event)
                        .ok()
                        .and_then(|value| serde_json::from_value(value).ok())
                })
                .collect())
        })
    }

    fn names(
        &self,
        pubkeys: Vec<String>,
        _members: Vec<String>,
    ) -> BoxFuture<'static, HashMap<String, String>> {
        let profiles = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.borrow().profiles.clone())
            .unwrap_or_default();
        let mut names = HashMap::new();
        let mut missing = Vec::new();
        for pubkey in pubkeys {
            match profiles.get(&pubkey) {
                Some(profile) if !profile.name_is_fallback => {
                    names.insert(pubkey, profile.name.clone());
                }
                _ => missing.push(pubkey),
            }
        }
        let http = self.http.clone();
        Box::pin(async move {
            if missing.is_empty() {
                return names;
            }
            let filter =
                serde_json::json!({ "kinds": [0], "authors": missing, "limit": missing.len() });
            if let Ok(events) = http.query(&filters(vec![filter])).await {
                let mut newest: HashMap<String, (u64, String)> = HashMap::new();
                for event in events {
                    let Some(name) = profile_name(&event.content) else {
                        continue;
                    };
                    let (pubkey, at) = (event.pubkey.to_hex(), event.created_at.as_secs());
                    if newest.get(&pubkey).is_none_or(|(seen, _)| *seen < at) {
                        newest.insert(pubkey, (at, name));
                    }
                }
                names.extend(newest.into_iter().map(|(pubkey, (_, name))| (pubkey, name)));
            }
            names
        })
    }

    fn authority(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?.borrow().clone();
        snapshot
            .relay
            .as_ref()
            .map(|relay| relay.authority.clone())
            .filter(|authority| !authority.is_empty())
    }

    fn profile_owner(&self, pubkey: &str) -> BoxFuture<'static, Option<String>> {
        let (http, pubkey) = (self.http.clone(), pubkey.to_string());
        Box::pin(async move {
            let filter = serde_json::json!({ "kinds": [0], "authors": [pubkey], "limit": 500 });
            let events = http.query(&filters(vec![filter])).await.ok()?;
            // Signatures are checked as the events are read.
            let newest = events
                .into_iter()
                .filter(|event| event.kind.as_u16() == 0 && event.pubkey.to_hex() == pubkey)
                .max_by_key(|event| event.created_at.as_secs())?;
            let tags: Vec<Vec<String>> = newest
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect();
            buzz_relay_client::profiles::attested_owner_of(
                &pubkey,
                0,
                newest.created_at.as_secs(),
                &tags,
            )
        })
    }

    fn me_chat(&self, owner: &str, channel: &str) -> BoxFuture<'static, Result<bool, String>> {
        let session = self.session.as_ref().and_then(Weak::upgrade);
        let dm = self.channel(channel).is_some_and(|info| info.dm);
        let (Some(session), false, false) = (session, dm, self.identity.pubkey_hex() != owner)
        else {
            return Box::pin(async { Ok(false) });
        };
        let (kit, weak, channel) = (
            session.me_kit().clone(),
            Arc::downgrade(&session),
            channel.to_string(),
        );
        Box::pin(async move {
            let status = || kit.snapshot().status.clone();
            if status() == MeKitStatus::Unavailable {
                return Ok(false);
            }
            if status() != MeKitStatus::Ready {
                kit.refresh().await;
            }
            if weak.upgrade().is_none() {
                return Err("The community changed before this was answered. Try again.".into());
            }
            if status() != MeKitStatus::Ready {
                let snapshot = kit.snapshot();
                return Err(format!(
                    "Couldn't read your Me chats to tell where to answer ({}). Try again.",
                    snapshot.error().unwrap_or("not loaded")
                ));
            }
            Ok(kit.placed(&channel))
        })
    }

    fn memories(&self, agent: &str) -> BoxFuture<'static, Result<Vec<Memory>, String>> {
        let read = self.memory_listing(agent);
        Box::pin(async move { read.await.map(|(memories, _)| memories) })
    }

    fn memory_listing(
        &self,
        agent: &str,
    ) -> BoxFuture<'static, Result<(Vec<Memory>, bool), String>> {
        let (http, identity, agent) = (self.http.clone(), self.identity.clone(), agent.to_string());
        Box::pin(async move {
            let query = buzz_desktop::relay::agent::memory_query(&identity.pubkey_hex(), &agent)?;
            let values: Vec<Value> =
                serde_json::from_str(&query).map_err(|_| "Invalid memory target")?;
            let records = http
                .query_raw(&filters(values))
                .await
                .map_err(|error| error.to_string())?;
            let records: Vec<Option<Value>> = records.into_iter().map(Some).collect();
            let listing = identity.decode_agent_memories(&agent, &records)?;
            let partial = listing["partial"].as_bool().unwrap_or(true);
            let memories = listing
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            Some(Memory {
                                slug: entry.get("slug")?.as_str()?.to_string(),
                                body: entry.get("body")?.as_str()?.to_string(),
                                created_at: entry.get("createdAt")?.as_u64()?,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok((memories, partial))
        })
    }
}

/// A community as its owner reads it, and its scope, `<origin>:<viewer>`.
pub type Scoped = (String, Arc<dyn OwnerRelay>);

/// A community's reader, kept while its owner and session stay the same:
/// each is asked for on every agent change, and each builds an HTTP
/// client.
struct Cached {
    identity: Weak<Identity>,
    session: Weak<Session>,
    scoped: Scoped,
}

static CACHE: Mutex<Option<HashMap<EntityId, Cached>>> = Mutex::new(None);

/// A community, read as its owner, and its relay partition (buzz-app's
/// relay `scope`, `<origin>:<viewer>`). `None` until the owner is signed
/// in.
pub fn of(community: &Entity<Community>, cx: &App) -> Option<Scoped> {
    let IdentityState::Ready(identity) = &cx.global::<AppState>().identity else {
        return None;
    };
    let id = community.entity_id();
    let community = community.read(cx);
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache.get_or_insert_default();
    if let Some(cached) = cache.get(&id)
        && cached.identity.ptr_eq(&Arc::downgrade(identity))
        && cached.session.ptr_eq(&Arc::downgrade(&community.session))
    {
        return Some(cached.scoped.clone());
    }
    let partition = community.partition();
    let _runtime = Runtime::handle(cx).enter();
    let relay: Arc<dyn OwnerRelay> = Arc::new(AppRelay {
        http: RelayHttp::new(community.origin.clone(), identity.clone()),
        identity: identity.clone(),
        snapshot: Some(community.session.snapshot()),
        session: Some(Arc::downgrade(&community.session)),
    });
    let scoped = (format!("{}:{}", partition.origin, partition.viewer), relay);
    cache.insert(
        id,
        Cached {
            identity: Arc::downgrade(identity),
            session: Arc::downgrade(&community.session),
            scoped: scoped.clone(),
        },
    );
    Some(scoped)
}

/// The owner's connection to each running agent's own community, which
/// need not be the selected one, by agent pubkey: buzz-app's
/// `agents2.relay(pubkey)`. Agents of one community share it.
pub fn running(agents: &Agents2, cx: &App) -> HashMap<String, Scoped> {
    let mut communities: HashMap<EntityId, Option<Scoped>> = HashMap::new();
    let relays = agents
        .running()
        .iter()
        .filter_map(|agent| {
            let community = agents.relay(&agent.pubkey)?;
            let relay = communities
                .entry(community.entity_id())
                .or_insert_with(|| of(&community, cx))
                .clone()?;
            Some((agent.pubkey.clone(), relay))
        })
        .collect();
    // Communities no running agent uses any more let go of their readers.
    if let Some(cache) = CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        cache.retain(|id, _| communities.contains_key(id));
    }
    relays
}
