//! Profiles (kind 0) for everyone the UI shows (spec §5f).
//!
//! Only the newest kind 0 per author counts. Unchanged profiles keep their
//! `Arc`, so views can tell at a glance whether anything they show moved.

use crate::budget::ByteLru;
use crate::event::{Verified, is_newer, kinds};
use crate::socket::{Filter, filter};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Authors per read.
pub const CHUNK: usize = 500;
/// Profiles retained per session.
pub const CAPACITY: usize = 1024;
/// How long an author whose read found no profile is not asked again.
const ABSENT_FOR: Duration = Duration::from_secs(120);
/// Authors remembered as having no profile.
const ABSENT_CAPACITY: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Profile {
    pub name: String,
    /// `name` is the pubkey prefix because the profile has no usable name.
    pub name_is_fallback: bool,
    /// An `https:` URL or an inline raster `data:` URL; anything else is
    /// dropped so the UI shows initials.
    pub picture: Option<String>,
    pub about: Option<String>,
    /// Self-declared and not DNS-verified.
    pub nip05: Option<String>,
    pub is_agent: bool,
    /// From a NIP-OA `auth` tag, by shape only.
    pub owner_pubkey: Option<String>,
    /// The owner whose attestation verifies ([`attested_owner`]); only
    /// this one may act as the agent's owner.
    pub attested_owner: Option<String>,
}

impl Profile {
    /// What to show for an author with no profile yet.
    pub fn fallback(pubkey: &str) -> Self {
        Self {
            name: pubkey.chars().take(10).collect(),
            name_is_fallback: true,
            ..Default::default()
        }
    }

    /// Parses a kind 0 (`profiles.ts:7-73`).
    pub fn from_event(event: &Verified) -> Self {
        let pubkey = event.pubkey_hex();
        let auth: Option<&[String]> = event
            .tags_named("auth")
            .find(|tag| tag.len() == 4 && is_hex(&tag[1], 64) && is_hex(&tag[3], 128));
        let owner_pubkey = auth.map(|tag| tag[1].clone());
        let attested_owner = owner_pubkey.as_ref().and_then(|_| attested_owner(event));
        let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(&event.content) else {
            return Self {
                is_agent: owner_pubkey.is_some(),
                owner_pubkey,
                attested_owner,
                ..Self::fallback(&pubkey)
            };
        };
        let text = |key: &str| {
            fields
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        // A name is kept as written; only a blank one is skipped.
        let named = |key: &str| {
            fields
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        };
        let name = named("display_name").or_else(|| named("name"));
        let flagged = |key: &str| fields.get(key) == Some(&Value::Bool(true));
        Self {
            name_is_fallback: name.is_none(),
            name: name.unwrap_or_else(|| pubkey.chars().take(10).collect()),
            picture: text("picture").filter(|url| avatar_source(url)),
            about: text("about"),
            nip05: text("nip05"),
            is_agent: flagged("is_agent") || flagged("isAgent") || owner_pubkey.is_some(),
            owner_pubkey,
            attested_owner,
        }
    }
}

// ---------------------------------------------------------------------------
// NIP-OA owner attestation (`owner-attestation.ts`)

/// `^(kind=|created_at<|created_at>)(0|[1-9][0-9]*)$`, bounded.
fn clause(clause: &str) -> Option<(&'static str, u64)> {
    let (name, digits) = ["kind=", "created_at<", "created_at>"]
        .into_iter()
        .find_map(|name| Some((name, clause.strip_prefix(name)?)))?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    let value: u64 = digits.parse().ok().filter(|_| canonical)?;
    let limit = if name == "kind=" {
        65_535
    } else {
        4_294_967_295
    };
    (value <= limit).then_some((name, value))
}

/// Every clause is valid and holds for this event; empty always holds.
fn conditions_hold(conditions: &str, kind: u16, created_at: u64) -> bool {
    if conditions.is_empty() {
        return true;
    }
    let clauses: Option<Vec<(&str, u64)>> = conditions.split('&').map(clause).collect();
    clauses.is_some_and(|clauses| {
        clauses.iter().all(|(name, value)| match *name {
            "kind=" => u64::from(kind) == *value,
            "created_at<" => created_at < *value,
            _ => created_at > *value,
        })
    })
}

/// The verified NIP-OA owner of an agent's signature-checked event: its
/// one `["auth", owner, conditions, sig]` tag, with conditions that hold
/// for the event and a BIP-340 signature by `owner` over
/// `sha256("nostr:agent-auth:<agent>:<conditions>")`. Provenance only: the
/// event stays authored by the agent.
pub fn attested_owner(event: &Verified) -> Option<String> {
    let tags: Vec<Vec<String>> = event.tags_named("auth").map(<[String]>::to_vec).collect();
    attested_owner_of(
        &event.pubkey_hex(),
        event.kind_u16(),
        event.created_at_secs(),
        &tags,
    )
}

/// [`attested_owner`] of an event the caller already checked the
/// signature of, by its author, kind, time and tags.
pub fn attested_owner_of(
    target: &str,
    kind: u16,
    created_at: u64,
    tags: &[Vec<String>],
) -> Option<String> {
    let mut auth = tags
        .iter()
        .filter(|tag| tag.first().map(String::as_str) == Some("auth"));
    let tag = auth.next()?;
    if auth.next().is_some() {
        return None;
    }
    auth_owner(target, tag, |conditions| {
        conditions_hold(conditions, kind, created_at)
    })
}

/// The owner one `["auth", owner, conditions, sig]` tag attests for
/// `target` (`authTagOwner`): well-formed conditions and a valid owner
/// signature, whatever kind or time they name. NIP-IA archival ignores
/// kind clauses and checks time bounds when the request is signed
/// ([`within_time_bounds`]).
pub fn auth_tag_owner(target: &str, tag: &[String]) -> Option<String> {
    auth_owner(target, tag, |conditions| {
        conditions.is_empty() || conditions.split('&').all(|part| clause(part).is_some())
    })
}

/// Whether `conditions`' time clauses allow an event made at
/// `created_at` (`withinTimeBounds`); kind clauses are not checked.
pub fn within_time_bounds(conditions: &str, created_at: u64) -> bool {
    if conditions.is_empty() {
        return true;
    }
    let clauses: Option<Vec<(&str, u64)>> = conditions.split('&').map(clause).collect();
    clauses.is_some_and(|clauses| {
        clauses.iter().all(|(name, value)| match *name {
            "kind=" => true,
            "created_at<" => created_at < *value,
            _ => created_at > *value,
        })
    })
}

fn auth_owner(target: &str, tag: &[String], holds: impl Fn(&str) -> bool) -> Option<String> {
    use secp256k1::{Secp256k1, XOnlyPublicKey, schnorr::Signature};
    use sha2::{Digest, Sha256};
    if tag.len() != 4 || tag[0] != "auth" {
        return None;
    }
    let (owner, conditions, sig) = (&tag[1], &tag[2], &tag[3]);
    if !is_hex(owner, 64) || !is_hex(sig, 128) || owner == target || !holds(conditions) {
        return None;
    }
    let digest: [u8; 32] = Sha256::digest(format!("nostr:agent-auth:{target}:{conditions}")).into();
    let signature: Signature = sig.parse().ok()?;
    let key: XOnlyPublicKey = owner.parse().ok()?;
    Secp256k1::verification_only()
        .verify_schnorr(&signature, &digest, &key)
        .ok()
        .map(|_| owner.clone())
}

/// `avatar-source.ts`: credential-free HTTPS up to 2048 characters, or an
/// inline PNG/JPEG/WebP/GIF up to 512 KiB.
fn avatar_source(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        return url.len() <= 2048 && !authority.is_empty() && !authority.contains('@');
    }
    ["png", "jpeg", "webp", "gif"].iter().any(|format| {
        url.strip_prefix(&format!("data:image/{format};base64,"))
            .is_some_and(|_| url.len() <= 512 * 1024)
    })
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub type Profiles = Arc<HashMap<String, Arc<Profile>>>;

/// Bytes of retained profile events (`profile-directory.ts`).
pub const MAX_BYTES: usize = 2 << 20;

pub struct ProfileDirectory {
    /// Winning signed kind 0 per author, least recently updated first out.
    events: ByteLru<Verified>,
    snapshot: Profiles,
    in_flight: HashSet<String>,
    /// Authors a read found no profile for, and until when they are not
    /// asked again. A failed read proves nothing, so it marks no one.
    absent: HashMap<String, Instant>,
}

impl Default for ProfileDirectory {
    fn default() -> Self {
        Self {
            events: ByteLru::new(CAPACITY, MAX_BYTES),
            snapshot: Profiles::default(),
            in_flight: HashSet::new(),
            absent: HashMap::new(),
        }
    }
}

impl ProfileDirectory {
    pub fn snapshot(&self) -> Profiles {
        self.snapshot.clone()
    }

    /// The winning signed profile event retained for `pubkey`.
    pub fn event(&self, pubkey: &str) -> Option<&Verified> {
        self.events.peek(pubkey)
    }

    /// Filters for the ids not already known, being fetched or recently
    /// found absent, in chunks of 500 authors. The ids are marked in
    /// flight until [`Self::finished`].
    pub fn ensure<'a>(
        &mut self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Vec<(Vec<String>, Filter)> {
        let now = Instant::now();
        let mut missing: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                is_hex(id, 64)
                    && !self.events.contains(id)
                    && !self.in_flight.contains(*id)
                    && self.absent.get(*id).is_none_or(|until| *until <= now)
            })
            .map(str::to_string)
            .collect();
        missing.sort();
        missing.dedup();
        self.in_flight.extend(missing.iter().cloned());
        missing
            .chunks(CHUNK)
            .map(|chunk| (chunk.to_vec(), authors_filter(chunk)))
            .collect()
    }

    /// Re-reads every known id, e.g. after a reconnect. Current profiles
    /// stay until something newer arrives.
    pub fn refresh(&mut self) -> Vec<(Vec<String>, Filter)> {
        self.absent.clear();
        let mut known: Vec<String> = self.events.keys();
        known.sort();
        self.in_flight.extend(known.iter().cloned());
        known
            .chunks(CHUNK)
            .map(|chunk| (chunk.to_vec(), authors_filter(chunk)))
            .collect()
    }

    /// Ends a read of `ids`. After a successful read its events must be
    /// given to [`Self::accept`] next; until then every id without a
    /// profile counts as absent. A failed read only clears the ids.
    pub fn finished(&mut self, ids: &[String], succeeded: bool) {
        if !succeeded {
            for id in ids {
                self.in_flight.remove(id);
            }
            return;
        }
        let now = Instant::now();
        if self.absent.len() + ids.len() > ABSENT_CAPACITY {
            self.absent.retain(|_, until| *until > now);
        }
        for id in ids {
            self.in_flight.remove(id);
            if !self.events.contains(id) && self.absent.len() < ABSENT_CAPACITY {
                self.absent.insert(id.clone(), now + ABSENT_FOR);
            }
        }
    }

    /// Takes any kind 0 events; returns whether the snapshot changed.
    pub fn accept(&mut self, events: &[Verified]) -> bool {
        let mut changed: Vec<String> = Vec::new();
        for event in events.iter().filter(|e| e.kind_u16() == kinds::PROFILE) {
            let pubkey = event.pubkey_hex();
            self.absent.remove(&pubkey);
            let newer = self
                .events
                .peek(&pubkey)
                .is_none_or(|current| is_newer(event, current));
            if newer {
                self.events
                    .set(pubkey.clone(), event.clone(), event.byte_size());
                changed.push(pubkey);
            }
        }
        if changed.is_empty() {
            return false;
        }
        let mut next = (*self.snapshot).clone();
        let mut different = false;
        // Whatever the budgets evicted leaves the snapshot too.
        next.retain(|id, _| {
            let kept = self.events.contains(id);
            different |= !kept;
            kept
        });
        for id in changed {
            let Some(event) = self.events.peek(&id) else {
                continue;
            };
            let profile = Profile::from_event(event);
            if next.get(&id).is_none_or(|current| **current != profile) {
                next.insert(id, Arc::new(profile));
                different = true;
            }
        }
        if different {
            self.snapshot = Arc::new(next);
        }
        different
    }

    /// Undoes an optimistic profile that the relay did not take: if
    /// `pubkey`'s profile is still the event `failed`, it goes back to
    /// `previous` (or to unknown). Returns whether the snapshot changed.
    pub fn revert(
        &mut self,
        pubkey: &str,
        failed: &nostr::prelude::EventId,
        previous: Option<Verified>,
    ) -> bool {
        if self
            .events
            .peek(pubkey)
            .is_none_or(|event| event.id != *failed)
        {
            return false;
        }
        let mut next = (*self.snapshot).clone();
        match previous {
            Some(event) => {
                next.insert(pubkey.to_string(), Arc::new(Profile::from_event(&event)));
                let bytes = event.byte_size();
                self.events.set(pubkey.to_string(), event, bytes);
            }
            None => {
                self.events.remove(pubkey);
                next.remove(pubkey);
            }
        }
        self.snapshot = Arc::new(next);
        true
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.in_flight.clear();
        self.absent.clear();
        self.snapshot = Profiles::default();
    }
}

fn authors_filter(authors: &[String]) -> Filter {
    filter(json!({"kinds": [kinds::PROFILE], "authors": authors, "limit": authors.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    fn profile(keys: &Keys, content: &str, at: u64, tags: Vec<Tag>) -> Verified {
        let event = EventBuilder::new(Kind::from_u16(0), content)
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(keys)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    #[test]
    fn the_directory_keeps_two_mebibytes_of_the_newest_profiles() {
        let mut directory = ProfileDirectory::default();
        let about = "x".repeat(400 * 1024);
        let keys: Vec<Keys> = (0..6).map(|_| Keys::generate()).collect();
        for (n, keys) in keys.iter().enumerate() {
            let content = format!(r#"{{"name":"p{n}","about":"{about}"}}"#);
            assert!(directory.accept(&[profile(keys, &content, 1, vec![])]));
        }
        // Five of about 400 KiB fit; the first one went.
        let first = keys[0].public_key().to_hex();
        assert!(directory.event(&first).is_none());
        assert!(!directory.snapshot().contains_key(&first));
        let last = keys[5].public_key().to_hex();
        assert!(directory.snapshot().contains_key(&last));
        assert_eq!(directory.snapshot().len(), 5);
    }

    #[test]
    fn parses_names_pictures_and_agents() {
        let keys = Keys::generate();
        let parsed = Profile::from_event(&profile(
            &keys,
            r#"{"name":"alice","display_name":"  Alice ","picture":"https://x.example/a.png","about":" "}"#,
            1,
            vec![],
        ));
        assert_eq!(parsed.name, "  Alice ");
        assert!(!parsed.name_is_fallback);
        assert_eq!(parsed.picture.as_deref(), Some("https://x.example/a.png"));
        assert_eq!(parsed.about, None);

        let unsafe_picture = Profile::from_event(&profile(
            &keys,
            r#"{"name":"a","picture":"https://u:p@x.example/a"}"#,
            1,
            vec![],
        ));
        assert_eq!(unsafe_picture.picture, None);

        let agent = Profile::from_event(&profile(
            &keys,
            "not json",
            1,
            vec![Tag::custom(
                "auth",
                ["a".repeat(64), String::new(), "b".repeat(128)],
            )],
        ));
        assert!(agent.is_agent && agent.name_is_fallback);
        assert_eq!(agent.owner_pubkey, Some("a".repeat(64)));
        assert_eq!(
            agent.attested_owner, None,
            "a bogus signature attests nothing"
        );
    }

    /// An `auth` tag signed by `owner` for `agent` under `conditions`.
    fn auth(owner: &Keys, agent: &Keys, conditions: &str) -> Tag {
        use sha2::{Digest, Sha256};
        let agent = agent.public_key().to_hex();
        let digest: [u8; 32] =
            Sha256::digest(format!("nostr:agent-auth:{agent}:{conditions}")).into();
        Tag::custom(
            "auth",
            [
                owner.public_key().to_hex(),
                conditions.to_owned(),
                owner.sign_schnorr(digest).to_string(),
            ],
        )
    }

    #[test]
    fn verifies_owner_attestations() {
        let (owner, agent) = (buzz_identity::Identity::ephemeral(), Keys::generate());
        let tag: Vec<String> =
            serde_json::from_str(&owner.attest_agent(&agent.public_key().to_hex()).unwrap())
                .unwrap();
        let signed = profile(&agent, "{}", 100, vec![Tag::parse(tag).unwrap()]);
        assert_eq!(attested_owner(&signed), Some(owner.pubkey_hex()));
        let parsed = Profile::from_event(&signed);
        assert_eq!(parsed.attested_owner, Some(owner.pubkey_hex()));
        assert!(parsed.is_agent);

        let (owner, other) = (Keys::generate(), Keys::generate());
        let owner_hex = owner.public_key().to_hex();
        // Conditions are bound into the signature and must hold.
        let bounded = auth(&owner, &agent, "kind=0&created_at<200");
        let current = profile(&agent, "{}", 100, vec![bounded.clone()]);
        assert_eq!(attested_owner(&current), Some(owner_hex));
        assert_eq!(
            attested_owner(&profile(&agent, "{}", 300, vec![bounded])),
            None
        );
        let wrong_kind = profile(&agent, "{}", 100, vec![auth(&owner, &agent, "kind=1")]);
        assert_eq!(attested_owner(&wrong_kind), None);
        for bad in [
            "kind=01",
            "kind=65536",
            "created_at=5",
            "kind=0&",
            "created_at>-1",
        ] {
            let event = profile(&agent, "{}", 100, vec![auth(&owner, &agent, bad)]);
            assert_eq!(attested_owner(&event), None, "{bad}");
        }
        // Signed for another agent, or two auth tags: not attested.
        let stolen = profile(&other, "{}", 100, vec![auth(&owner, &agent, "")]);
        assert_eq!(attested_owner(&stolen), None);
        let twice = vec![auth(&owner, &agent, ""), auth(&other, &agent, "")];
        assert_eq!(attested_owner(&profile(&agent, "{}", 100, twice)), None);
        // An agent cannot own itself.
        let own = profile(&agent, "{}", 100, vec![auth(&agent, &agent, "")]);
        assert_eq!(attested_owner(&own), None);
    }

    #[test]
    fn keeps_the_newest_and_shares_unchanged_profiles() {
        let keys = Keys::generate();
        let id = keys.public_key().to_hex();
        let mut directory = ProfileDirectory::default();
        assert!(directory.accept(&[profile(&keys, r#"{"name":"new"}"#, 10, vec![])]));
        let before = directory.snapshot()[&id].clone();
        assert!(!directory.accept(&[profile(&keys, r#"{"name":"old"}"#, 5, vec![])]));
        assert_eq!(directory.snapshot()[&id].name, "new");
        // A newer event with identical fields leaves the snapshot alone.
        assert!(!directory.accept(&[profile(&keys, r#"{"name":"new"}"#, 11, vec![])]));
        assert!(Arc::ptr_eq(&before, &directory.snapshot()[&id]));
    }

    #[test]
    fn reverts_only_the_failed_optimistic_profile() {
        let keys = Keys::generate();
        let id = keys.public_key().to_hex();
        let mut directory = ProfileDirectory::default();
        let saved = profile(&keys, r#"{"name":"saved"}"#, 1, vec![]);
        let optimistic = profile(&keys, r#"{"name":"draft"}"#, 2, vec![]);
        directory.accept(std::slice::from_ref(&saved));
        directory.accept(std::slice::from_ref(&optimistic));
        // A different event id is not the failed one.
        assert!(!directory.revert(&id, &saved.id, Some(saved.clone())));
        assert!(directory.revert(&id, &optimistic.id, Some(saved.clone())));
        assert_eq!(directory.snapshot()[&id].name, "saved");
        directory.accept(std::slice::from_ref(&optimistic));
        assert!(directory.revert(&id, &optimistic.id, None));
        assert!(!directory.snapshot().contains_key(&id));
    }

    #[test]
    fn ensure_skips_known_and_in_flight_ids() {
        let keys = Keys::generate();
        let known = keys.public_key().to_hex();
        let mut directory = ProfileDirectory::default();
        directory.accept(&[profile(&keys, r#"{"name":"k"}"#, 1, vec![])]);
        let other = "c".repeat(64);
        let reads = directory.ensure([known.as_str(), other.as_str(), "short"]);
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].0, vec![other.clone()]);
        assert!(directory.ensure([other.as_str()]).is_empty());
        directory.finished(std::slice::from_ref(&other), false);
        // A failed read proves nothing, so the id is asked again.
        assert_eq!(directory.ensure([other.as_str()]).len(), 1);
        directory.finished(std::slice::from_ref(&other), true);
        // A read that found nothing is not repeated until a refresh.
        assert!(directory.ensure([other.as_str()]).is_empty());
        directory.refresh();
        assert_eq!(directory.ensure([other.as_str()]).len(), 1);
        directory.finished(std::slice::from_ref(&other), true);
        let late = Keys::generate();
        let late_id = late.public_key().to_hex();
        assert_eq!(directory.ensure([late_id.as_str()]).len(), 1);
        directory.finished(std::slice::from_ref(&late_id), true);
        directory.accept(&[profile(&late, r#"{"name":"l"}"#, 1, vec![])]);
        assert!(!directory.absent.contains_key(&late_id));
    }
}
