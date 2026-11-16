//! Owner-written team status. No observer, transcript, or runner inputs.
//! Intent is saved before signing; uncertain writes remain clearable.

use super::Session;
use super::actions::{Context, read};
use super::util::{now, publish, sign};
use crate::{
    event::{Verifier, kinds},
    messages::EventTemplate,
    socket::filter,
    store::Store,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

const KIND: u16 = kinds::TEAM_AGENT_STATUS;
const CAPACITY: usize = 32;
const JOURNAL: &str = "team-status-v1";
// All owner clients in this process serialize sends, including community sessions.
// The native app excludes a second process in the same profile.
// Other profiles and devices require the owner's explicit coordination.
static SEND: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Owner assertion, never inferred from agent activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Owner reports ongoing work.
    Working,
    /// Owner reports waiting.
    Waiting,
    /// Owner reports a blocker.
    Blocked,
    /// Owner reports completion, without independently verifying it.
    Done,
}
impl Status {
    /// Exact wire prefix.
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

/// Immutable originating task scope, resolved and verified by this session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Canonical authenticated relay community UUID.
    pub community: String,
    /// Subject identity.
    pub agent: String,
    /// Originating channel, never a separately selected audience.
    pub channel: String,
    /// Originating conversation root.
    pub root: String,
}
impl Scope {
    fn address(&self) -> String {
        format!(
            "team-status:{}:{}:{}:{}",
            self.community, self.agent, self.channel, self.root
        )
    }
}

/// Exact owner-facing preview. Private fields prevent changing its audience.
#[derive(Clone, Debug)]
pub struct Preview {
    scope: Scope,
    label: String,
    generation: u64,
    origin: String,
    owner: String,
}
impl Preview {
    /// Approved originating tuple.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
    /// Exact public enum and optional caption.
    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    scope: Scope,
    origin: String,
    // None is a durable clear intent. Empty desired content is never a label.
    desired: Option<String>,
    #[serde(default)]
    generation: u64,
    last: u64,
    pending: bool,
    error: Option<String>,
}

/// Owner-visible durable state, including offline clears.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// Number of approved, enabled addresses.
    pub sharing: usize,
    /// Writes or clears that still need relay acceptance.
    pub pending: usize,
    /// Most recent delivery or storage error.
    pub error: Option<String>,
}

pub(super) struct Publisher {
    store: Store,
    viewer: String,
    key: String,
    entries: Mutex<Result<Vec<Entry>, String>>,
    generation: AtomicU64,
    next_attempt: Mutex<HashMap<String, u64>>,
    runtime_error: Mutex<Option<String>>,
}

impl Publisher {
    pub(super) fn shared(store: Store, viewer: String) -> Arc<Self> {
        static OWNERS: OnceLock<Mutex<HashMap<String, Weak<Publisher>>>> = OnceLock::new();
        let mut owners = OWNERS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        owners.retain(|_, p| p.strong_count() > 0);
        if let Some(p) = owners.get(&viewer).and_then(Weak::upgrade) {
            return p;
        }
        let publisher = Self::new(store, viewer.clone());
        owners.insert(viewer, Arc::downgrade(&publisher));
        publisher
    }

    fn new(store: Store, viewer: String) -> Arc<Self> {
        let key = JOURNAL.to_owned();
        let entries = (|| {
            let raw = store
                .viewer_pref(&viewer, &key)
                .map_err(|e| e.to_string())?;
            let mut entries: Vec<Entry> = raw
                .map(|s| serde_json::from_str(&s))
                .transpose()
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            if entries.len() > CAPACITY {
                return Err("Team status journal exceeds capacity".into());
            }
            // Restart never silently resumes sharing. Retain every possible send.
            for entry in &mut entries {
                if entry.desired.take().is_some() {
                    entry.pending = true;
                }
            }
            store
                .set_viewer_pref(
                    &viewer,
                    &key,
                    &serde_json::to_string(&entries).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            Ok(entries)
        })();
        Arc::new(Self {
            store,
            viewer,
            key,
            entries: Mutex::new(entries),
            generation: AtomicU64::new(0),
            next_attempt: Mutex::new(HashMap::new()),
            runtime_error: Mutex::new(None),
        })
    }

    fn edit<T>(&self, f: impl FnOnce(&mut Vec<Entry>) -> Result<T, String>) -> Result<T, String> {
        let mut guard = self
            .entries
            .lock()
            .map_err(|_| "Team status journal lock failed")?;
        let entries = guard.as_mut().map_err(|e| e.clone())?;
        let mut next = entries.clone();
        let result = f(&mut next)?;
        self.store
            .set_viewer_pref(
                &self.viewer,
                &self.key,
                &serde_json::to_string(&next).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        *entries = next;
        Ok(result)
    }

    fn state(&self) -> State {
        match self.entries.lock() {
            Ok(guard) => match guard.as_ref() {
                Ok(entries) => State {
                    sharing: entries.iter().filter(|e| e.desired.is_some()).count(),
                    pending: entries.iter().filter(|e| e.pending).count(),
                    error: entries
                        .iter()
                        .find_map(|e| e.error.clone())
                        .or_else(|| self.runtime_error.lock().ok().and_then(|e| e.clone())),
                },
                Err(e) => State {
                    error: Some(e.clone()),
                    ..Default::default()
                },
            },
            Err(_) => State {
                error: Some("Team status journal lock failed".into()),
                ..Default::default()
            },
        }
    }

    fn approve(&self, preview: Preview, confirmed: bool) -> Result<(), String> {
        if preview.owner != self.viewer {
            return Err("Preview belongs to another owner".into());
        }
        if !confirmed {
            return Err(
                "Confirm this thread and caption describe shareable work in its own channel".into(),
            );
        }
        self.edit(|entries| {
            if preview.generation != self.generation.load(Ordering::SeqCst) {
                return Err("Sharing was stopped. Preview again to enable it.".into());
            }
            let index = entries.iter().position(|e| e.scope == preview.scope);
            if index.is_none() && entries.len() >= CAPACITY {
                return Err("Team status address capacity reached".into());
            }
            let entry = match index {
                Some(i) => &mut entries[i],
                None => {
                    entries.push(Entry {
                        scope: preview.scope,
                        origin: preview.origin,
                        desired: None,
                        generation: 0,
                        last: 0,
                        pending: false,
                        error: None,
                    });
                    entries.last_mut().ok_or("Missing status intent")?
                }
            };
            if entry.pending {
                return Err("Wait for the pending write or clear before sharing again".into());
            }
            entry.generation = preview.generation;
            entry.desired = Some(preview.label);
            entry.pending = true;
            entry.error = None;
            Ok(())
        })
    }

    pub(super) fn stop(&self) -> Result<(), String> {
        // Fence even when persistence fails: no further labels may leave.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.edit(|entries| {
            for entry in entries {
                if entry.desired.take().is_some() || entry.pending {
                    entry.pending = true;
                    entry.error = None;
                }
            }
            Ok(())
        })
    }

    fn next(&self, origin: &str, at: u64) -> Result<Option<Entry>, String> {
        self.edit(|entries| {
            let Some(entry) = entries
                .iter_mut()
                .filter(|e| e.pending && e.origin == origin)
                .min_by_key(|e| e.last)
            else {
                return Ok(None);
            };
            // Strictly newer than EVERY attempted send, including uncertain sends.
            let timestamp = at.max(entry.last.saturating_add(1));
            if timestamp > at.saturating_add(60) {
                return Err("Clock moved backwards; clear remains pending".into());
            }
            entry.last = timestamp;
            Ok(Some(entry.clone()))
        })
    }

    fn after_head(&self, sent: &Entry, head: u64) -> Result<Entry, String> {
        self.edit(|entries| {
            let entry = entries
                .iter_mut()
                .find(|e| e.scope == sent.scope && e.last == sent.last && e.desired == sent.desired)
                .ok_or("Intent changed while checking the relay")?;
            entry.last = entry.last.max(head.saturating_add(1));
            if entry.last > now().saturating_add(60) {
                return Err("Relay watermark is ahead of this clock; retry pending".into());
            }
            Ok(entry.clone())
        })
    }

    fn finish(&self, sent: &Entry, result: Result<(), String>) -> Result<(), String> {
        self.edit(|entries| {
            if let Some(entry) = entries
                .iter_mut()
                .find(|e| e.scope == sent.scope && e.last == sent.last && e.desired == sent.desired)
            {
                entry.pending = result.is_err();
                entry.error = result.err();
            }
            Ok(())
        })
    }
}

fn label(status: Status, caption: &str) -> Result<String, String> {
    let label = if caption.is_empty() {
        status.label().to_owned()
    } else {
        format!("{}: {caption}", status.label())
    };
    if label.chars().count() > 96
        || caption.trim() != caption
        || label
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        || label.contains(['@', '/', '\\', '`'])
        || label.to_ascii_lowercase().contains("nostr:")
    {
        return Err("Use a public caption without mentions, paths, links, commands, or line breaks (96 characters total)".into());
    }
    Ok(label)
}

fn template(entry: &Entry) -> Result<EventTemplate, String> {
    let s = &entry.scope;
    let tags = vec![
        vec!["d".into(), s.address()],
        vec!["community".into(), s.community.clone()],
        vec!["h".into(), s.channel.clone()],
        vec!["p".into(), s.agent.clone()],
        vec!["e".into(), s.root.clone(), "".into(), "root".into()],
        vec![
            "expiration".into(),
            entry.last.saturating_add(900).to_string(),
        ],
    ];
    let content = entry
        .desired
        .as_ref()
        .map(|label| json!({"v":1,"label":label}).to_string())
        .unwrap_or_default();
    if content.len() > 512 {
        return Err("Team status content exceeds 512 bytes".into());
    }
    Ok(EventTemplate {
        kind: KIND,
        created_at: entry.last,
        tags,
        content,
    })
}

async fn community(ctx: &Context) -> Result<String, String> {
    // Authenticated response is authoritative, never a hostname or a UI UUID.
    let bytes = ctx
        .http
        .post(
            "/query",
            serde_json::to_string(&crate::read_state::snapshot_filter(&ctx.viewer))
                .map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| e.to_string())?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let id = raw["community_id"]
        .as_str()
        .ok_or("Relay did not resolve a community UUID")?;
    if uuid::Uuid::parse_str(id)
        .map_err(|e| e.to_string())?
        .to_string()
        != id
    {
        return Err("Noncanonical community UUID".into());
    }
    crate::read_state::parse_snapshot_response(&raw, &ctx.viewer, id, &Verifier::default())
        .map_err(|e| e.to_string())?;
    Ok(id.to_owned())
}

async fn preflight(ctx: &Context, scope: &Scope) -> Result<(), String> {
    if community(ctx).await? != scope.community {
        return Err("Originating community changed".into());
    }
    super::agent_sharing::profile(ctx, &scope.agent).await?;
    let state = super::agent_sharing::state(ctx, &scope.channel).await?;
    if state.channel_type == crate::lifecycle::StateType::Dm
        || state.archived
        || !state.roles.contains_key(&ctx.viewer)
        || !state.roles.contains_key(&scope.agent)
    {
        return Err("Status needs a live channel with owner and agent membership".into());
    }
    let events = read(ctx, vec![filter(json!({"ids":[scope.root], "kinds":[kinds::CHAT,kinds::AGENT_MESSAGE,kinds::DIFF,kinds::FORUM_POST], "limit":1, "consistency":"strong"}))]).await?;
    let root = events
        .iter()
        .find(|e| e.id.to_hex() == scope.root && e.tag("h") == Some(scope.channel.as_str()))
        .ok_or("Originating root is unavailable")?;
    if root
        .tags_named("e")
        .any(|t| t.len() < 4 || t.get(3).is_some_and(|m| m == "root" || m == "reply"))
        || root.tag("deleted").is_some()
    {
        return Err("Status must reference a live top-level conversation".into());
    }
    Ok(())
}

impl Session {
    /// Resolve and verify an originating thread and exact manual status preview.
    pub async fn preview_team_status(
        &self,
        agent: &str,
        channel: &str,
        root: &str,
        status: Status,
        caption: &str,
    ) -> Result<Preview, String> {
        if !crate::event::is_hex64(agent)
            || !crate::event::is_hex64(root)
            || uuid::Uuid::parse_str(channel).is_err()
        {
            return Err("Invalid originating tuple".into());
        }
        let label = label(status, caption)?;
        let (ops, publisher) = (self.ops(), self.team_status.clone());
        let (agent, channel, root) = (agent.to_owned(), channel.to_owned(), root.to_owned());
        let origin = self.origin.http();
        let generation = publisher.generation.load(Ordering::SeqCst);
        self.run(async move {
            let ctx = ops.context().await?;
            let scope = Scope {
                community: community(&ctx).await?,
                agent,
                channel,
                root,
            };
            preflight(&ctx, &scope).await?;
            Ok(Preview {
                scope,
                label,
                generation,
                origin,
                owner: ctx.viewer,
            })
        })
        .await
    }

    /// Publish precisely the preview after explicit owner shareability confirmation.
    pub async fn share_team_status(&self, preview: Preview, confirmed: bool) -> Result<(), String> {
        let (ops, publisher) = (self.ops(), self.team_status.clone());
        self.run(async move {
            let ctx = ops.context().await?;
            preflight(&ctx, &preview.scope).await?;
            publisher.approve(preview, confirmed)
        })
        .await
    }

    /// Stop this owner immediately and durably queue clears across originating communities.
    pub fn stop_team_status(&self) -> Result<(), String> {
        self.team_status.stop()
    }

    /// Current durable sharing and retry state for this owner.
    pub fn team_status_state(&self) -> State {
        self.team_status.state()
    }
}

/// One bounded worker per community session; nothing uses the ordinary tagged outbox.
pub(super) fn spawn(
    runtime: &tokio::runtime::Handle,
    publisher: Arc<Publisher>,
    ops: super::actions::Ops,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        loop {
            // Rate bound: at most one attempted write every 6 seconds, also on failure.
            tokio::time::sleep(Duration::from_secs(6)).await;
            if publisher.state().pending == 0 { continue; }
            let Ok(_serial) = SEND.try_lock() else {
                continue;
            };
            let work = async {
                let ctx = ops.context().await?;
                let at = now();
                let origin = ctx.http.origin().http();
                let due = publisher.next_attempt.lock().map_err(|_| "Retry clock lock failed")?.get(&origin).copied().unwrap_or(0);
                if at < due { return Ok(()); }
                let Some(mut entry) = publisher.next(&origin, at)? else {
                    return Ok(());
                };
                publisher.next_attempt.lock().map_err(|_| "Retry clock lock failed")?.insert(origin, at.saturating_add(6));
                let result = async {
                    // Clears also need the same current authority; failures retain intent.
                    preflight(&ctx, &entry.scope).await?;
                    if entry.desired.is_some()
                        && publisher.generation.load(Ordering::SeqCst) != entry.generation
                    {
                        return Err("Sharing stopped; clear pending".into());
                    }
                    let heads = read(&ctx, vec![filter(json!({"kinds":[KIND],"authors":[ctx.viewer],"#d":[entry.scope.address()],"limit":1,"consistency":"strong"}))]).await?;
                    let head = heads.iter().filter(|e| e.pubkey_hex() == ctx.viewer && e.tag("d") == Some(entry.scope.address().as_str())).map(|e| e.created_at_secs()).max().unwrap_or(0);
                    entry = publisher.after_head(&entry, head)?;
                    let event = sign(&ctx.identity, template(&entry)?)?;
                    publish(&ctx.http, &event).await
                }
                .await;
                publisher.finish(&entry, result)
            };
            // A timeout leaves the already journaled timestamp and intent for retry.
            let outcome = tokio::time::timeout(Duration::from_secs(30), work).await;
            if let Ok(mut error) = publisher.runtime_error.lock() {
                *error = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(message)) => Some(message),
                    Err(_) => Some("Team status request timed out; retry remains pending".into()),
                };
            }
        }
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod session_tests;
