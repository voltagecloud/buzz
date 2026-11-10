//! NIP-IG integration event tags — the shared contract between integration
//! publishers (such as `buzz-ingest`) and consumers (the workflow engine).
//!
//! An integration event is an ordinary channel event (a kind:9 message, or a
//! kind:30623 [`crate::kind::KIND_INTEGRATION_RECORD`]) that mirrors an
//! occurrence in an external system: a Linear issue comment, a GitHub check
//! run, a Slack message. Its typed metadata lives in `ext-*` tags:
//!
//! ```text
//! ["ext-source", "linear"]           provider
//! ["ext-action", "issue.labeled"]    normalized "<object>.<verb>"
//! ["ext-id", "<provider object id>"]
//! ["ext-url", "https://linear.app/..."]
//! ["ext-actor", "<provider login or id>"]
//! ["ext-thread", "<thread key>"]
//! ["ext-key", "<dedup key>"]
//! ["ext-labels", "agent"]             repeatable
//! ```
//!
//! Consumers read these generically: every `ext-<name>` tag becomes the field
//! `<name>` (dashes folded to underscores), and repeated tags join with `,`.
//! Nothing here knows about any provider. See `docs/nips/NIP-IG.md`.
//!
//! The tags are signer-controlled metadata. A consumer that acts on them must
//! first check that the signer is a trusted integration publisher.

use std::collections::BTreeMap;

/// Prefix shared by every integration metadata tag.
pub const EXT_TAG_PREFIX: &str = "ext-";
/// Provider name, for example `github`, `linear`, or `slack`.
pub const TAG_SOURCE: &str = "ext-source";
/// Normalized action, `<object>.<verb>`, for example `pull_request.opened`.
pub const TAG_ACTION: &str = "ext-action";
/// Provider-side identifier of the object the occurrence is about.
pub const TAG_ID: &str = "ext-id";
/// Canonical provider URL of the object.
pub const TAG_URL: &str = "ext-url";
/// Provider-side actor (login, user id) who caused the occurrence.
pub const TAG_ACTOR: &str = "ext-actor";
/// Key of the external thread (issue, pull request, Slack thread).
pub const TAG_THREAD: &str = "ext-thread";
/// Stable dedup key of the occurrence; equal to the record's `d` tag.
pub const TAG_KEY: &str = "ext-key";
/// Repository, as `owner/name` (GitHub).
pub const TAG_REPO: &str = "ext-repo";
/// Team or workspace key (Linear team key, Slack team id).
pub const TAG_TEAM: &str = "ext-team";
/// Human reference, for example `ENG-123` or `owner/repo#42`.
pub const TAG_REF: &str = "ext-ref";
/// Title of the object the thread mirrors.
pub const TAG_TITLE: &str = "ext-title";
/// Current workflow state (Linear state name, GitHub open/closed/merged).
pub const TAG_STATE: &str = "ext-state";
/// A label currently on the object. Repeatable.
pub const TAG_LABEL: &str = "ext-labels";
/// A label this occurrence added. Repeatable.
pub const TAG_ADDED_LABEL: &str = "ext-added-labels";
/// CI conclusion (`success`, `failure`, ...).
pub const TAG_CONCLUSION: &str = "ext-conclusion";
/// Review state (`approved`, `changes_requested`, `commented`).
pub const TAG_REVIEW_STATE: &str = "ext-review-state";
/// Provider channel id (Slack channel).
pub const TAG_CHANNEL: &str = "ext-channel";

/// On a kind:30623 record: the id of the kind:9 message that renders it.
///
/// Deliberately not an `e` tag. The record is written before its message
/// (so a retry can never orphan a message), and an `e` tag would make the
/// relay resolve NIP-10 thread metadata against an event that does not exist
/// yet.
pub const TAG_MESSAGE: &str = "message";

/// Most `ext-*` fields read from one event. Bounds work on a hostile event.
pub const MAX_FIELDS: usize = 32;

/// Most bytes kept per field value, after joining repeated tags.
pub const MAX_FIELD_BYTES: usize = 4096;

/// Read every `ext-*` tag of `tags` into a field map.
///
/// - `ext-added-labels` becomes the key `added_labels`.
/// - Repeated tags join their values with `,` in tag order.
/// - Names that would not be a plain identifier (ASCII letters, digits,
///   underscore after folding) are skipped, so every key is usable as an
///   evalexpr variable suffix.
/// - At most [`MAX_FIELDS`] keys; values are cut at [`MAX_FIELD_BYTES`] on a
///   character boundary.
pub fn ext_fields(tags: &nostr::Tags) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for tag in tags.iter() {
        let parts = tag.as_slice();
        let (Some(name), Some(value)) = (parts.first(), parts.get(1)) else {
            continue;
        };
        let Some(raw_key) = name.strip_prefix(EXT_TAG_PREFIX) else {
            continue;
        };
        let key = raw_key.replace('-', "_");
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        if !fields.contains_key(&key) && fields.len() >= MAX_FIELDS {
            continue;
        }
        let slot = fields.entry(key).or_default();
        if !slot.is_empty() {
            slot.push(',');
        }
        slot.push_str(value);
        truncate_on_char_boundary(slot, MAX_FIELD_BYTES);
    }
    fields
}

/// True when `tags` carry both an `ext-source` and an `ext-action` value.
pub fn is_integration_event(tags: &nostr::Tags) -> bool {
    let has = |name: &str| {
        tags.iter().any(|t| {
            let parts = t.as_slice();
            parts.first().map(String::as_str) == Some(name)
                && parts.get(1).is_some_and(|v| !v.is_empty())
        })
    };
    has(TAG_SOURCE) && has(TAG_ACTION)
}

fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(raw: &[&[&str]]) -> nostr::Tags {
        nostr::Tags::from_list(
            raw.iter()
                .map(|t| nostr::Tag::parse(t.iter().copied()).unwrap())
                .collect(),
        )
    }

    #[test]
    fn ext_fields_folds_names_and_joins_repeats() {
        let t = tags(&[
            &["h", "chan"],
            &["ext-source", "linear"],
            &["ext-action", "issue.labeled"],
            &["ext-labels", "agent"],
            &["ext-labels", "bug"],
            &["ext-added-labels", "agent"],
            &["message", "abc"],
        ]);
        let f = ext_fields(&t);
        assert_eq!(f.get("source").map(String::as_str), Some("linear"));
        assert_eq!(f.get("action").map(String::as_str), Some("issue.labeled"));
        assert_eq!(f.get("labels").map(String::as_str), Some("agent,bug"));
        assert_eq!(f.get("added_labels").map(String::as_str), Some("agent"));
        assert!(!f.contains_key("h"));
        assert!(!f.contains_key("message"));
    }

    #[test]
    fn ext_fields_skips_non_identifier_names_and_bounds_output() {
        let mut raw: Vec<Vec<String>> = vec![
            vec!["ext-".into(), "empty".into()],
            vec!["ext-a.b".into(), "dot".into()],
            vec!["ext-bad name".into(), "space".into()],
        ];
        for i in 0..(MAX_FIELDS + 5) {
            raw.push(vec![format!("ext-f{i}"), "v".into()]);
        }
        raw.push(vec!["ext-big".into(), "é".repeat(MAX_FIELD_BYTES)]);
        let t = nostr::Tags::from_list(
            raw.iter()
                .map(|t| nostr::Tag::parse(t.iter().map(String::as_str)).unwrap())
                .collect(),
        );
        let f = ext_fields(&t);
        assert_eq!(f.len(), MAX_FIELDS);
        assert!(f.keys().all(|k| !k.is_empty() && !k.contains('.')));

        let big = tags(&[&["ext-big", &"é".repeat(MAX_FIELD_BYTES)]]);
        let f = ext_fields(&big);
        let v = f.get("big").unwrap();
        assert!(v.len() <= MAX_FIELD_BYTES);
        assert!(v.chars().all(|c| c == 'é'));
    }

    #[test]
    fn is_integration_event_requires_source_and_action() {
        assert!(is_integration_event(&tags(&[
            &["ext-source", "github"],
            &["ext-action", "pull_request.opened"],
        ])));
        assert!(!is_integration_event(&tags(&[&["ext-source", "github"]])));
        assert!(!is_integration_event(&tags(&[
            &["ext-source", "github"],
            &["ext-action", ""],
        ])));
    }
}
