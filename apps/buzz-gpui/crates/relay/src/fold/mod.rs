//! Message folding: one window's verified events become timeline rows.
//!
//! A port of buzz-app's `fold.ts` and its helpers (`docs/port/relay.md`
//! §5b.2–5b.5, §5c.1, §5c.3). Everything here is pure and synchronous. Rows
//! are immutable and shared as `Arc<MessageRow>`; [`MessageProjection`]
//! keeps a row's `Arc` while its folded value is unchanged, so the UI can
//! skip re-rendering it (`row-identity.ts`).

mod attachments;
mod membership;
mod order;
mod projection;
mod reply_tree;
mod thread;

#[cfg(test)]
mod tests;

pub use attachments::{
    Attachment, AttachmentKind, Dimensions, message_markdown, parse_attachments,
    visible_html_source,
};
pub use membership::{MembershipChange, MembershipKind, membership_change};
pub use order::{MessageClock, compare_messages, event_ms};
pub use projection::MessageProjection;
pub use reply_tree::{ReplyTree, reply_tree};
pub use thread::{ThreadReference, thread_reference};

use crate::audience::{Audience, audience};
use crate::event::{Verified, kinds};
use nostr::prelude::{Event, EventId, PublicKey};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Kinds that are timeline rows (`membership.ts:6-8`).
pub fn is_row_kind(kind: u16) -> bool {
    kinds::CHANNEL_ROW_KINDS.contains(&kind)
}

/// Outbox state of a locally sent row (§6.3). The projection only carries
/// it; the outbox owns the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Delivery {
    Sending,
    Accepted,
    Unknown,
    Failed,
    Seen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDelivery {
    pub status: Delivery,
    pub error: Option<String>,
}

/// An event-local custom emoji mapping. Messages render emoji from their own
/// tags, never from the live community catalog (§5k.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CustomEmoji {
    pub shortcode: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaction {
    pub id: EventId,
    /// The raw signer: only it may delete the reaction.
    pub author: PublicKey,
}

/// Reactions with the same content and emoji URL. Counted per event; the UI
/// counts distinct authors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionGroup {
    pub content: String,
    pub emoji: Option<CustomEmoji>,
    pub events: Vec<Reaction>,
}

/// Kind 40008 tags. Untrusted display metadata; the content stays a raw patch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffMeta {
    pub file_path: Option<String>,
    pub repo_url: Option<String>,
    pub commit_sha: Option<String>,
    pub description: Option<String>,
    pub truncated: bool,
}

/// A top-level copy of a thread reply (`["buzz:sent-from-thread", root,
/// excerpt?]`, §5d.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentFromThread {
    pub root_id: EventId,
    /// At most 64 code points.
    pub excerpt: Option<String>,
}

/// One folded timeline row. Equality is deep, which is what lets the
/// projection reuse an unchanged row's `Arc`.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRow {
    pub id: EventId,
    pub channel: Arc<str>,
    /// The original event's kind, regardless of edits. 40002 marks an agent
    /// envelope and 40008 a diff; both are display hints, not authority.
    pub kind: u16,
    /// The original event's signed audience: agents is coordination
    /// between agents, shown quietly. `None` is everyone; unknown values
    /// too.
    pub audience: Option<Audience>,
    /// Displayed author ([`message_author`]).
    pub author: PublicKey,
    /// The signing key, when relay attribution makes it differ from `author`.
    pub signer: Option<PublicKey>,
    /// Display-only workflow owner attested by the relay's signing authority.
    pub workflow_owner: Option<PublicKey>,
    pub created_at: u64,
    /// `created_at * 1000` plus a valid `ms` tag; the rendered order key.
    pub created_at_ms: u64,
    /// The current body with projected attachment Markdown removed.
    pub content: String,
    /// The unprojected current body, when projection changed it.
    pub source_content: Option<String>,
    /// An authorized edit replaced the body.
    pub edited: bool,
    /// Byte offsets in `content` where removed attachment Markdown joined
    /// once-separate text. A bound mention never spans one.
    pub attachment_seams: Vec<usize>,
    /// The edit that supplied the current `imeta`, when not the original.
    pub attachment_source_id: Option<EventId>,
    pub attachments: Vec<Attachment>,
    pub emoji: Vec<CustomEmoji>,
    /// Notified recipients (`p` tags), always from the original event.
    pub mentions: Vec<PublicKey>,
    /// Display-only references (two-element `mention` tags).
    pub mention_references: Vec<PublicKey>,
    pub reactions: Vec<ReactionGroup>,
    pub reply_count: u64,
    pub participants: Vec<PublicKey>,
    pub thread_root_id: Option<EventId>,
    pub reply_parent_id: Option<EventId>,
    pub sent_from_thread: Option<SentFromThread>,
    pub diff: Option<DiffMeta>,
    /// Set only on relay-signed 40099 activity rows, which have no body.
    pub membership: Option<MembershipChange>,
    /// Local outbox state; `None` for rows that only came from the relay.
    pub delivery: Option<LocalDelivery>,
}

impl MessageRow {
    pub fn is_agent_envelope(&self) -> bool {
        self.kind == kinds::AGENT_MESSAGE
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldOptions {
    /// Keep replies as rows (thread views and session channels). Otherwise a
    /// reply is a row only with `["broadcast","1"]`.
    pub include_replies: bool,
    /// The relay's NIP-11 `self`. Events it signs are attributed through
    /// `actor` or a leading `p` tag. `None` (a contact-key fallback) leaves
    /// every signer as the author.
    pub signing_authority: Option<PublicKey>,
    /// The channel's admins from its relay-signed 39001.
    ///
    /// Spec §9.3 #7: the relay soft-deletes the target of an admin's 9005,
    /// but the reference fold applies a 9005 only when its signer authored
    /// the target, so a live moderator delete stayed visible until a head
    /// reload. The port also applies a 9005 that carries this channel's `h`
    /// tag and is signed by a key in this set. Empty keeps parity.
    pub channel_admins: HashSet<PublicKey>,
}

/// `content` as a row shows it: images the event carries as attachments
/// leave the text.
pub(crate) fn displayed_text(event: &Event, content: &str) -> String {
    let urls = attachments::imeta_urls(event);
    if content.contains("![") || !urls.is_empty() {
        attachments::project_markdown(content, &urls).content
    } else {
        content.to_owned()
    }
}

/// The displayed author (`fold.ts:199-219`). Output signed by the relay's
/// signing authority names its author in `actor`, else in a leading `p` tag
/// of a channel event. Any other signer is the author.
pub fn message_author(event: &Event, signing_authority: Option<&PublicKey>) -> PublicKey {
    if signing_authority != Some(&event.pubkey) {
        return event.pubkey;
    }
    let actor = event.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..] if name == "actor" => pubkey(value),
        _ => None,
    });
    if let Some(actor) = actor {
        return actor;
    }
    let leading = match event.tags.first().map(|tag| tag.as_slice()) {
        Some([name, value, ..]) if name == "p" => pubkey(value),
        _ => None,
    };
    match leading {
        Some(author) if event.tags.iter().any(|tag| tag.kind() == "h") => author,
        _ => event.pubkey,
    }
}

/// Whether `deletion` removes `target` (`fold.ts:221-243`, rules a/b/c in
/// §5b.5), plus the admin 9005 rule described on
/// [`FoldOptions::channel_admins`].
///
/// Rule (c) has no author check: a kind 5 with a matching `k` tag reached us
/// only after relay ingest checked author or agent owner. Owner attribution
/// applies to message kinds only; reactions keep raw-signer semantics.
pub fn deletion_applies(
    deletion: &Event,
    target: &Event,
    channel: &str,
    options: &FoldOptions,
) -> bool {
    let kind = deletion.kind.as_u16();
    let nip09 = kind == kinds::DELETION || kind == kinds::MODERATOR_DELETE;
    if !nip09 {
        return false;
    }
    let message = matches!(target.kind.as_u16(), kinds::CHAT | kinds::AGENT_MESSAGE);
    deletion.pubkey == target.pubkey
        || (message
            && deletion.pubkey == message_author(target, options.signing_authority.as_ref()))
        || (kind == kinds::DELETION
            && message
            && has_tag(deletion, "k", &target.kind.as_u16().to_string()))
        || (kind == kinds::MODERATOR_DELETE
            && options.channel_admins.contains(&deletion.pubkey)
            && has_tag(deletion, "h", channel))
}

/// Folds one window's rows with their overlays (`fold.ts:245-430`): author
/// deletes, author edits (latest wins), reactions and relay-signed thread
/// summaries. Output is in [`compare_messages`] order.
pub fn fold_messages(
    channel: &str,
    relay_author: &PublicKey,
    events: &[Verified],
    options: &FoldOptions,
) -> Vec<Arc<MessageRow>> {
    fold_rows(&Arc::from(channel), relay_author, events, options)
        .into_iter()
        .map(Arc::new)
        .collect()
}

pub(crate) fn fold_rows(
    channel: &Arc<str>,
    relay_author: &PublicKey,
    events: &[Verified],
    options: &FoldOptions,
) -> Vec<MessageRow> {
    let fold = Fold::index(channel, relay_author, events, options);
    let mut rows: Vec<MessageRow> = events.iter().filter_map(|event| fold.row(event)).collect();
    rows.sort_by(compare_messages);
    rows
}

struct Fold<'a> {
    channel: &'a Arc<str>,
    relay_author: &'a PublicKey,
    options: &'a FoldOptions,
    overlays: HashMap<EventId, Vec<&'a Verified>>,
    summaries: HashMap<EventId, (&'a Verified, Summary)>,
}

impl<'a> Fold<'a> {
    fn index(
        channel: &'a Arc<str>,
        relay_author: &'a PublicKey,
        events: &'a [Verified],
        options: &'a FoldOptions,
    ) -> Self {
        let mut overlays: HashMap<EventId, Vec<&Verified>> = HashMap::new();
        let mut summaries: HashMap<EventId, (&Verified, Summary)> = HashMap::new();
        for event in events {
            let kind = event.kind_u16();
            if is_row_kind(kind) {
                continue;
            }
            if kind == kinds::THREAD_SUMMARY {
                // Keyed by the first `e` tag only, and only from the relay.
                let target = event.tag("e").and_then(event_id);
                if let Some(target) = target
                    && event.pubkey == *relay_author
                {
                    let summary = Summary::parse(event);
                    match summaries.get(&target) {
                        Some((current, _)) if !summary_wins(event, current) => {}
                        _ => {
                            summaries.insert(target, (event, summary));
                        }
                    }
                }
                continue;
            }
            for target in e_targets(event) {
                let list = overlays.entry(target).or_default();
                if !list.last().is_some_and(|last| last.id == event.id) {
                    list.push(event);
                }
            }
        }
        Self {
            channel,
            relay_author,
            options,
            overlays,
            summaries,
        }
    }

    fn aux(&self, id: &EventId) -> &[&'a Verified] {
        self.overlays.get(id).map_or(&[], Vec::as_slice)
    }

    fn deleted(&self, event: &Event) -> bool {
        self.aux(&event.id)
            .iter()
            .any(|item| deletion_applies(item, event, self.channel, self.options))
    }

    fn row(&self, event: &Verified) -> Option<MessageRow> {
        let kind = event.kind_u16();
        if !is_row_kind(kind) || !has_tag(event, "h", self.channel) {
            return None;
        }
        let reply = event
            .tags
            .iter()
            .any(|tag| matches!(tag.as_slice(), [name, _, _, marker, ..] if name == "e" && marker == "reply"));
        if !self.options.include_replies && reply && !has_tag(event, "broadcast", "1") {
            return None;
        }
        if self.deleted(event) {
            return None;
        }
        if kind == kinds::SYSTEM_MESSAGE {
            let membership = membership_change(event, self.relay_author)?;
            return Some(MessageRow {
                membership: Some(membership),
                ..self.bare(event)
            });
        }

        let authority = self.options.signing_authority.as_ref();
        let author = message_author(event, authority);
        let aux = self.aux(&event.id);
        let mut edits: Vec<&Verified> = aux
            .iter()
            .copied()
            .filter(|item| {
                item.kind_u16() == kinds::EDIT
                    && (item.pubkey == event.pubkey || item.pubkey == author)
                    && !self.deleted(item)
            })
            .collect();
        edits.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        let current = edits.first().copied();

        let mut content = current.map_or(&event.content, |edit| &edit.content).clone();
        if kind == kinds::AGENT_MESSAGE
            && let Some(Value::String(inner)) =
                object_body(&content).and_then(|mut body| body.remove("content"))
        {
            content = inner;
        }

        let attachment_event = edits
            .iter()
            .copied()
            .find(|edit| edit.tags.iter().any(|tag| tag.kind() == "imeta"))
            .unwrap_or(event);
        let imeta_urls = attachments::imeta_urls(attachment_event);
        // Every CommonMark image starts with `![`, and an attachment link
        // needs an imeta URL, so ordinary messages skip the parse.
        let projected = if kind != kinds::DIFF && (content.contains("![") || !imeta_urls.is_empty())
        {
            attachments::project_markdown(&content, &imeta_urls)
        } else {
            attachments::Projected::unchanged(&content)
        };
        let mut names: HashMap<String, String> = HashMap::new();
        for (url, name) in projected.names {
            names.entry(url).or_insert(name);
        }
        let attachments = parse_attachments(attachment_event, &projected.urls, &names);
        let emoji_source = current
            .filter(|edit| edit.tags.iter().any(|tag| tag.kind() == "emoji"))
            .unwrap_or(event);
        let summary = self
            .summaries
            .get(&event.id)
            .map(|(_, summary)| summary.clone())
            .unwrap_or_default();
        let reference = thread_reference(event);

        Some(MessageRow {
            signer: (author != event.pubkey).then_some(event.pubkey),
            author,
            workflow_owner: workflow_owner(event, authority),
            source_content: (projected.content != content).then(|| content.clone()),
            content: projected.content,
            edited: current.is_some(),
            attachment_seams: projected.seams,
            attachment_source_id: (attachment_event.id != event.id).then_some(attachment_event.id),
            attachments,
            emoji: emoji_tags(emoji_source),
            mentions: dedup(event.tags.iter().filter_map(|tag| match tag.as_slice() {
                [name, value, ..] if name == "p" => pubkey(value),
                _ => None,
            })),
            mention_references: dedup(event.tags.iter().filter_map(|tag| match tag.as_slice() {
                [name, value] if name == "mention" => pubkey(value),
                _ => None,
            })),
            reactions: group_reactions(
                aux.iter()
                    .copied()
                    .filter(|item| item.kind_u16() == kinds::REACTION),
                |reaction| self.deleted(reaction),
            ),
            reply_count: summary.reply_count,
            participants: summary.participants,
            thread_root_id: reference.map(|r| r.root_id),
            reply_parent_id: reference.map(|r| r.parent_id),
            sent_from_thread: sent_from_thread(event),
            audience: audience(event),
            diff: (kind == kinds::DIFF).then(|| DiffMeta {
                file_path: event.tag("file").map(str::to_owned),
                repo_url: event.tag("repo").map(str::to_owned),
                commit_sha: event.tag("commit").map(str::to_owned),
                description: event.tag("description").map(str::to_owned),
                truncated: has_tag(event, "truncated", "true"),
            }),
            ..self.bare(event)
        })
    }

    /// A row with only the event's identity filled in.
    fn bare(&self, event: &Verified) -> MessageRow {
        MessageRow {
            id: event.id,
            channel: self.channel.clone(),
            kind: event.kind_u16(),
            audience: None,
            author: event.pubkey,
            signer: None,
            workflow_owner: None,
            created_at: event.created_at_secs(),
            created_at_ms: event_ms(event),
            content: String::new(),
            source_content: None,
            edited: false,
            attachment_seams: Vec::new(),
            attachment_source_id: None,
            attachments: Vec::new(),
            emoji: Vec::new(),
            mentions: Vec::new(),
            mention_references: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            participants: Vec::new(),
            thread_root_id: None,
            reply_parent_id: None,
            sent_from_thread: None,
            diff: None,
            membership: None,
            delivery: None,
        }
    }
}

/// Groups reactions (`fold.ts:432-487`). The group key is the content and
/// the custom emoji URL; groups are ordered by their first reaction, deleted
/// ones included, so a pill keeps its place when counts change.
pub fn group_reactions<'a>(
    reactions: impl IntoIterator<Item = &'a Verified>,
    deleted: impl Fn(&Verified) -> bool,
) -> Vec<ReactionGroup> {
    struct Building {
        group: ReactionGroup,
        first: (u64, EventId),
    }
    let mut keys: HashMap<(String, Option<String>), usize> = HashMap::new();
    let mut groups: Vec<Building> = Vec::new();
    for event in reactions {
        let lowered = event.content.to_lowercase();
        let emoji = emoji_tags(event)
            .into_iter()
            .find(|entry| lowered == format!(":{}:", entry.shortcode));
        let content = emoji
            .as_ref()
            .map_or_else(|| event.content.clone(), |e| format!(":{}:", e.shortcode));
        let key = (content.clone(), emoji.as_ref().map(|e| e.url.clone()));
        let at = (event.created_at_secs(), event.id);
        let index = *keys.entry(key).or_insert_with(|| {
            groups.push(Building {
                group: ReactionGroup {
                    content,
                    emoji,
                    events: Vec::new(),
                },
                first: at,
            });
            groups.len() - 1
        });
        let building = &mut groups[index];
        building.first = building.first.min(at);
        if !deleted(event) && !building.group.events.iter().any(|r| r.id == event.id) {
            building.group.events.push(Reaction {
                id: event.id,
                author: event.pubkey,
            });
        }
    }
    groups.retain(|b| !b.group.events.is_empty());
    groups.sort_by_key(|b| b.first);
    groups.into_iter().map(|b| b.group).collect()
}

/// Event-local emoji mappings (`emoji.ts:68-80`): first tag per normalized
/// shortcode wins, and URLs stay original.
pub fn emoji_tags(event: &Event) -> Vec<CustomEmoji> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for tag in event.tags.iter() {
        let [name, value, url, ..] = tag.as_slice() else {
            continue;
        };
        let url = url.trim();
        if name != "emoji" || value.is_empty() || url.is_empty() {
            continue;
        }
        if let Some(shortcode) = normalize_shortcode(value)
            && seen.insert(shortcode.clone())
        {
            result.push(CustomEmoji {
                shortcode,
                url: url.to_owned(),
            });
        }
    }
    result
}

fn normalize_shortcode(value: &str) -> Option<String> {
    let code = value.trim().trim_matches(':').to_lowercase();
    let valid = (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    valid.then_some(code)
}

/// `workflow-attribution.ts:5-26`. Every fold input is a [`Verified`]
/// event, so the reference's admission-proof check always holds here.
pub(crate) fn workflow_owner(event: &Event, authority: Option<&PublicKey>) -> Option<PublicKey> {
    if authority != Some(&event.pubkey) || event.kind.as_u16() != kinds::CHAT {
        return None;
    }
    let mut markers = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == "buzz:workflow");
    let mut owners = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == "buzz:workflow-owner");
    let (Some(marker), None, Some(owner), None) =
        (markers.next(), markers.next(), owners.next(), owners.next())
    else {
        return None;
    };
    match (marker.as_slice(), owner.as_slice()) {
        ([_, flag], [_, owner]) if flag == "true" => pubkey(owner),
        _ => None,
    }
}

fn sent_from_thread(event: &Event) -> Option<SentFromThread> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, root, rest @ ..] if name == "buzz:sent-from-thread" => {
            let root_id = event_id(root)?;
            let excerpt = rest
                .first()
                .filter(|excerpt| !excerpt.is_empty())
                .map(|excerpt| excerpt.chars().take(64).collect());
            Some(SentFromThread { root_id, excerpt })
        }
        _ => None,
    })
}

/// A parsed 39005 body (`fold.ts:167-197`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Summary {
    reply_count: u64,
    participants: Vec<PublicKey>,
}

impl Summary {
    fn parse(event: &Event) -> Self {
        let Some(body) = object_body(&event.content) else {
            return Self::default();
        };
        let reply_count = body
            .get("descendant_count")
            .and_then(non_negative_integer)
            .or_else(|| body.get("reply_count").and_then(non_negative_integer))
            .unwrap_or(0);
        let participants = match body.get("participants") {
            Some(Value::Array(values)) => {
                dedup(values.iter().filter_map(|v| v.as_str().and_then(pubkey)))
            }
            _ => Vec::new(),
        };
        Self {
            reply_count,
            participants,
        }
    }
}

/// The newer 39005 wins, and a same-second tie goes to the lower id, as in
/// the reference (`newer`, `fold.ts:257-273`). Spec §9.3 #14 notes this
/// can keep a stale count for a second; the next summary corrects it.
fn summary_wins(candidate: &Event, current: &Event) -> bool {
    candidate
        .created_at
        .cmp(&current.created_at)
        .then(current.id.cmp(&candidate.id))
        .is_gt()
}

/// JSON object bodies of 40002, 39005 and 40099 (`body.ts`).
fn object_body(content: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str(content) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// A JS `Number.isInteger(n) && n >= 0` check on a JSON value.
fn non_negative_integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n < u64::MAX as f64)
            .map(|n| n as u64)
    })
}

pub(crate) fn has_tag(event: &Event, name: &str, value: &str) -> bool {
    event
        .tags
        .iter()
        .any(|tag| matches!(tag.as_slice(), [n, v, ..] if n == name && v == value))
}

/// Distinct ids this event's `e` tags point at, in tag order.
pub(crate) fn e_targets(event: &Event) -> Vec<EventId> {
    let mut targets = Vec::new();
    for tag in event.tags.iter() {
        if let [name, value, ..] = tag.as_slice()
            && name == "e"
            && let Some(id) = event_id(value)
            && !targets.contains(&id)
        {
            targets.push(id);
        }
    }
    targets
}

fn dedup<T: Eq + std::hash::Hash + Copy>(values: impl Iterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    values.filter(|value| seen.insert(*value)).collect()
}

/// 32 bytes from 64 hex digits. Ids and keys in tags must be lowercase, as
/// in the reference; only NIP-10 parsing accepts uppercase.
fn hex32(value: &str, allow_upper: bool) -> Option<[u8; 32]> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' if allow_upper => Some(b - b'A' + 10),
        _ => None,
    };
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, [high, low]) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *byte = digit(*high)? << 4 | digit(*low)?;
    }
    Some(out)
}

fn event_id(value: &str) -> Option<EventId> {
    hex32(value, false).map(EventId::from_byte_array)
}

fn pubkey(value: &str) -> Option<PublicKey> {
    hex32(value, false).map(PublicKey::from_byte_array)
}
