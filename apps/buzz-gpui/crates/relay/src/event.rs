//! Events as the rest of the client sees them: verified once on arrival,
//! then shared cheaply.

use nostr::prelude::{Event, EventId};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::ops::Deref;
use std::sync::{Arc, Mutex};

/// Kinds and kind sets the client relies on. See `docs/port/relay.md` §0.2.
pub mod kinds {
    pub const PROFILE: u16 = 0;
    pub const DELETION: u16 = 5;
    pub const REACTION: u16 = 7;
    pub const CHAT: u16 = 9;
    pub const REPORT: u16 = 1984;
    pub const PUT_USER: u16 = 9000;
    pub const REMOVE_USER: u16 = 9001;
    pub const EDIT_CHANNEL: u16 = 9002;
    pub const MODERATOR_DELETE: u16 = 9005;
    pub const CREATE_CHANNEL: u16 = 9007;
    pub const DELETE_CHANNEL: u16 = 9008;
    pub const JOIN_CHANNEL: u16 = 9021;
    pub const LEAVE_CHANNEL: u16 = 9022;
    pub const AGENT_METADATA: u16 = 10100;
    pub const COMMUNITY_ROSTER: u16 = 13534;
    pub const IDENTITY_ARCHIVE: u16 = 13535;
    pub const PRESENCE: u16 = 20001;
    /// Encrypted agent activity frames (observer telemetry).
    pub const AGENT_ACTIVITY: u16 = 24200;
    pub const TYPING: u16 = 20002;
    pub const LEAVE_COMMUNITY: u16 = 28936;
    pub const CUSTOM_EMOJI: u16 = 30030;
    pub const APP_DATA: u16 = 30078;
    pub const MANAGED_AGENT: u16 = 30177;
    pub const REMINDERS: u16 = 30300;
    pub const USER_STATUS: u16 = 30315;
    pub const HIDDEN_DMS: u16 = 30622;
    pub const CHANNEL_METADATA: u16 = 39000;
    pub const CHANNEL_ADMINS: u16 = 39001;
    pub const CHANNEL_MEMBERS: u16 = 39002;
    pub const THREAD_SUMMARY: u16 = 39005;
    pub const CHANNEL_WINDOW: u16 = 39006;
    pub const THREAD_WINDOW: u16 = 39007;
    pub const AGENT_MESSAGE: u16 = 40002;
    pub const EDIT: u16 = 40003;
    pub const DIFF: u16 = 40008;
    pub const SYSTEM_MESSAGE: u16 = 40099;
    pub const CANVAS: u16 = 40100;
    pub const OPEN_DM: u16 = 41010;
    pub const HIDE_DM: u16 = 41012;
    pub const MEMBER_ADDED: u16 = 44100;
    pub const MEMBER_REMOVED: u16 = 44101;
    /// Encrypted agent turn metrics.
    pub const AGENT_METRICS: u16 = 44200;
    pub const FORUM_POST: u16 = 45001;
    pub const FORUM_COMMENT: u16 = 45003;

    /// Everything the live `#h` subscription asks for (`live.ts:200-203`),
    /// plus the 39001, whose admins' moderator deletes the fold applies.
    pub const CHANNEL_KINDS: &[u16] = &[
        CHAT,
        AGENT_MESSAGE,
        DIFF,
        FORUM_POST,
        FORUM_COMMENT,
        SYSTEM_MESSAGE,
        CANVAS,
        EDIT,
        DELETION,
        MODERATOR_DELETE,
        REACTION,
        CHANNEL_METADATA,
        CHANNEL_ADMINS,
        CHANNEL_MEMBERS,
        THREAD_SUMMARY,
        TYPING,
    ];

    /// Kinds that are timeline rows (`membership.ts:6-8`).
    pub const CHANNEL_ROW_KINDS: &[u16] = &[CHAT, AGENT_MESSAGE, DIFF, SYSTEM_MESSAGE];

    /// Kinds that only refer to other events and are visible only when
    /// their targets are (`event-access.ts`).
    pub const AUXILIARY_KINDS: &[u16] =
        &[DELETION, REACTION, MODERATOR_DELETE, EDIT, THREAD_SUMMARY];

    /// Kinds accepted only when signed by the relay's own key (§8.2).
    pub const RELAY_SIGNED_KINDS: &[u16] = &[
        CHANNEL_METADATA,
        CHANNEL_ADMINS,
        CHANNEL_MEMBERS,
        THREAD_SUMMARY,
        CHANNEL_WINDOW,
        THREAD_WINDOW,
        SYSTEM_MESSAGE,
        COMMUNITY_ROSTER,
        IDENTITY_ARCHIVE,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Relay supplied a malformed or invalidly signed event")]
pub struct InvalidEvent;

/// An event whose fields, ID hash and signature were checked. Only
/// [`Verifier::verify`] creates one.
#[derive(Clone)]
pub struct Verified(Arc<Inner>);

struct Inner {
    event: Event,
    /// Serialized size, measured once: budgets check it often.
    bytes: std::sync::OnceLock<usize>,
}

impl Deref for Verified {
    type Target = Event;
    fn deref(&self) -> &Event {
        &self.0.event
    }
}

impl std::fmt::Debug for Verified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Verified({} kind {})", self.id, self.kind)
    }
}

impl PartialEq for Verified {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Verified {}

impl Verified {
    fn new(event: Event) -> Self {
        Self(Arc::new(Inner {
            event,
            bytes: std::sync::OnceLock::new(),
        }))
    }

    pub fn event(&self) -> &Event {
        &self.0.event
    }

    /// Our own outbox intent before it is signed, so a send shows at once
    /// as a pending row (`outbox.ts` local snapshot). It carries no
    /// signature: only for display, never stored, sent or trusted as
    /// evidence of someone else's words.
    pub(crate) fn local(unsigned: &nostr::prelude::UnsignedEvent) -> Option<Self> {
        let id = unsigned.id?;
        let sig = nostr::prelude::Signature::from_slice(&[0; 64]).ok()?;
        Some(Self::new(Event::new(
            id,
            unsigned.pubkey,
            unsigned.created_at,
            unsigned.kind,
            unsigned.tags.clone(),
            unsigned.content.clone(),
            sig,
        )))
    }

    pub fn kind_u16(&self) -> u16 {
        self.kind.as_u16()
    }

    pub fn created_at_secs(&self) -> u64 {
        self.created_at.as_secs()
    }

    pub fn id_hex(&self) -> String {
        self.id.to_hex()
    }

    pub fn pubkey_hex(&self) -> String {
        self.pubkey.to_hex()
    }

    /// First value of the first tag with this name.
    pub fn tag(&self, name: &str) -> Option<&str> {
        first_tag(self, name)
    }

    /// All tags with this name, as slices including the name.
    pub fn tags_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a [String]> + 'a {
        self.tags
            .iter()
            .filter(move |tag| tag.kind() == name)
            .map(|tag| tag.as_slice())
    }

    pub fn has_tag(&self, name: &str, value: &str) -> bool {
        self.tags_named(name)
            .any(|tag| tag.get(1).map(String::as_str) == Some(value))
    }

    /// The channel this event belongs to: its single `h` tag, or the `d` tag
    /// of channel metadata and roster events. `None` when absent or ambiguous.
    pub fn channel(&self) -> Option<&str> {
        let mut h = self.tags_named("h").filter_map(|tag| tag.get(1));
        match (h.next(), h.next()) {
            (Some(channel), None) => Some(channel.as_str()),
            (Some(_), Some(_)) => None,
            (None, _) => matches!(
                self.kind_u16(),
                kinds::CHANNEL_METADATA | kinds::CHANNEL_ADMINS | kinds::CHANNEL_MEMBERS
            )
            .then(|| self.tag("d"))
            .flatten(),
        }
    }

    pub fn is_relay_signed_kind(&self) -> bool {
        kinds::RELAY_SIGNED_KINDS.contains(&self.kind_u16())
    }

    /// Serialized size, for retention budgets.
    pub fn byte_size(&self) -> usize {
        *self.0.bytes.get_or_init(|| self.0.event.as_json().len())
    }
}

/// Replaceable-event ordering (`events.ts:140-150`): the later `created_at`
/// wins; on a tie the lower ID wins, so arrival order cannot flip the result.
pub fn is_newer(candidate: &Event, current: &Event) -> bool {
    candidate.created_at > current.created_at
        || (candidate.created_at == current.created_at && candidate.id < current.id)
}

/// [`is_newer`] as a sort order: newest first.
pub(crate) fn newest_first(a: &Event, b: &Event) -> std::cmp::Ordering {
    b.created_at
        .cmp(&a.created_at)
        .then_with(|| a.id.cmp(&b.id))
}

/// The newest of `events` by [`is_newer`]; the first on a tie.
pub(crate) fn newest<'a, T>(events: impl IntoIterator<Item = &'a T>) -> Option<&'a T>
where
    T: Deref<Target = Event> + 'a,
{
    events.into_iter().fold(None, |best, event| match best {
        Some(best) if !is_newer(event, best) => Some(best),
        _ => Some(event),
    })
}

/// First value of the first tag named `name`.
pub(crate) fn first_tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == name)
        .and_then(|tag| tag.as_slice().get(1))
        .map(String::as_str)
}

/// 64 lowercase hex digits: the shape of event ids and pubkeys.
pub(crate) fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Checks events and remembers recent proofs, so an event seen on both the
/// socket and an HTTP read is not Schnorr-verified twice. A remembered
/// proof still requires the ID hash and signature to match exactly.
pub struct Verifier {
    proofs: Mutex<Proofs>,
}

struct Proofs {
    by_id: HashMap<EventId, nostr::prelude::Signature>,
    order: VecDeque<EventId>,
    capacity: usize,
}

impl Default for Verifier {
    fn default() -> Self {
        Self::new(2048)
    }
}

impl Verifier {
    pub fn new(capacity: usize) -> Self {
        Self {
            proofs: Mutex::new(Proofs {
                by_id: HashMap::with_capacity(capacity),
                order: VecDeque::with_capacity(capacity),
                capacity,
            }),
        }
    }

    /// Strict structural checks on the raw JSON, then ID and signature.
    pub fn verify_json(&self, value: &Value) -> Result<Verified, InvalidEvent> {
        check_shape(value)?;
        let event = Event::deserialize(value).map_err(|_| InvalidEvent)?;
        self.verify(event)
    }

    pub fn verify(&self, event: Event) -> Result<Verified, InvalidEvent> {
        if !event.verify_id() {
            return Err(InvalidEvent);
        }
        let remembered = self
            .proofs
            .lock()
            .expect("verifier lock")
            .by_id
            .get(&event.id)
            .is_some_and(|sig| *sig == event.sig);
        if !remembered {
            if !event.verify_signature() {
                return Err(InvalidEvent);
            }
            let mut proofs = self.proofs.lock().expect("verifier lock");
            if proofs.by_id.insert(event.id, event.sig).is_none() {
                proofs.order.push_back(event.id);
                if proofs.order.len() > proofs.capacity
                    && let Some(oldest) = proofs.order.pop_front()
                {
                    proofs.by_id.remove(&oldest);
                }
            }
        }
        Ok(Verified::new(event))
    }

    /// One bad event rejects the whole batch, as in the reference client.
    pub fn verify_all(&self, values: &[Value]) -> Result<Vec<Verified>, InvalidEvent> {
        values.iter().map(|value| self.verify_json(value)).collect()
    }
}

fn check_shape(value: &Value) -> Result<(), InvalidEvent> {
    let object = value.as_object().ok_or(InvalidEvent)?;
    let hex = |field: &str, len: usize| {
        object.get(field).and_then(Value::as_str).is_some_and(|s| {
            s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
    };
    let created_at = object
        .get("created_at")
        .and_then(Value::as_u64)
        .is_some_and(|t| t < (1 << 53));
    let kind = object
        .get("kind")
        .and_then(Value::as_u64)
        .is_some_and(|k| k <= 65535);
    let content = object.get("content").is_some_and(Value::is_string);
    let tags = object
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| {
            tags.iter().all(|tag| {
                tag.as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
            })
        });
    if hex("id", 64)
        && hex("pubkey", 64)
        && hex("sig", 128)
        && created_at
        && kind
        && content
        && tags
    {
        Ok(())
    } else {
        Err(InvalidEvent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag};
    use serde_json::json;

    fn event(tags: Vec<Tag>) -> Event {
        EventBuilder::new(Kind::from_u16(9), "hi")
            .tags(tags)
            .finalize(&Keys::generate())
            .unwrap()
    }

    #[test]
    fn rejects_tampering_and_bad_shapes() {
        let verifier = Verifier::default();
        let good = serde_json::to_value(event(Vec::new())).unwrap();
        assert!(verifier.verify_json(&good).is_ok());

        let mut tampered = good.clone();
        tampered["content"] = json!("changed");
        assert!(verifier.verify_json(&tampered).is_err());

        let mut upper = good.clone();
        upper["id"] = json!(good["id"].as_str().unwrap().to_uppercase());
        assert!(verifier.verify_json(&upper).is_err());

        let mut bad_tags = good.clone();
        bad_tags["tags"] = json!([["h", 1]]);
        assert!(verifier.verify_json(&bad_tags).is_err());

        // A remembered proof does not excuse a different signature.
        let mut forged_sig = good.clone();
        let sig = good["sig"].as_str().unwrap();
        let flipped = if sig.starts_with('0') { "1" } else { "0" };
        forged_sig["sig"] = json!(format!("{flipped}{}", &sig[1..]));
        assert!(verifier.verify_json(&forged_sig).is_err());

        assert!(verifier.verify_all(&[good.clone(), tampered]).is_err());
    }

    #[test]
    fn channel_comes_from_a_single_h_tag() {
        let verifier = Verifier::default();
        let one = verifier
            .verify(event(vec![Tag::custom("h", ["general"])]))
            .unwrap();
        assert_eq!(one.channel(), Some("general"));
        let two = verifier
            .verify(event(vec![
                Tag::custom("h", ["a"]),
                Tag::custom("h", ["b"]),
            ]))
            .unwrap();
        assert_eq!(two.channel(), None);
        let none = verifier.verify(event(Vec::new())).unwrap();
        assert_eq!(none.channel(), None);
    }

    #[test]
    fn replaceable_ties_go_to_the_lower_id() {
        let keys = Keys::generate();
        let at = nostr::prelude::Timestamp::from_secs(1_000);
        let a = EventBuilder::new(Kind::from_u16(0), "a")
            .custom_created_at(at)
            .finalize(&keys)
            .unwrap();
        let b = EventBuilder::new(Kind::from_u16(0), "b")
            .custom_created_at(at)
            .finalize(&keys)
            .unwrap();
        let (low, high) = if a.id < b.id { (a, b) } else { (b, a) };
        assert!(is_newer(&low, &high));
        assert!(!is_newer(&high, &low));
    }
}
