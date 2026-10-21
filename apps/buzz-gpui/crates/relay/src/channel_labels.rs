//! NIP-CL channel labels (`relay/channel-labels.ts`, #964): short tags
//! such as `for-you` that group channels and let a client find its own
//! channel again, plus the relay-written creator and creation time.
//!
//! Any channel admin can set labels, so they are grouping hints, never
//! trust. On relay-signed channel state the first `t` tag is the channel
//! type; later `t` tags are labels. A relay that does not advertise
//! `nip-cl` drops label tags, so label writes and lookups are refused
//! there before anything is signed.

use std::cmp::Ordering;

pub const MAX_CHANNEL_LABELS: usize = 8;
const CHANNEL_TYPES: [&str; 5] = ["stream", "forum", "dm", "workflow", "system"];

/// Refusal for label writes and lookups on a relay without `nip-cl`.
pub const UNSUPPORTED: &str = "This community's relay does not support channel labels yet.";
const INVALID: &str = "Use channel labels of 1–64 characters from a-z, 0-9, '.', ':' and '-', \
                       other than a channel type name.";

/// 1–64 characters from `a-z 0-9 . : -`, never a channel type name, so a
/// label cannot make a channel look like another type.
pub fn valid_label(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b':' | b'-'))
        && !CHANNEL_TYPES.contains(&value)
}

/// Canonical label tags for a 9007 or 9002: deduplicated, in caller order.
pub fn label_tags(labels: &[String]) -> Result<Vec<Vec<String>>, String> {
    let mut unique: Vec<&String> = Vec::new();
    for label in labels {
        if !unique.contains(&label) {
            unique.push(label);
        }
    }
    if unique.len() > MAX_CHANNEL_LABELS {
        return Err(format!("Use at most {MAX_CHANNEL_LABELS} channel labels."));
    }
    if !unique.iter().all(|label| valid_label(label)) {
        return Err(INVALID.into());
    }
    Ok(unique
        .into_iter()
        .map(|label| vec!["t".to_owned(), label.clone()])
        .collect())
}

/// The same label set, ignoring order and duplicates.
pub fn same_labels(a: &[String], b: &[String]) -> bool {
    a.iter().all(|label| b.contains(label)) && b.iter().all(|label| a.contains(label))
}

/// Relay-written identity facts of channel state (`channelIdentity`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelIdentity {
    pub channel_type: Option<String>,
    pub labels: Vec<String>,
    /// `P`: who created the channel (the 9007 signer).
    pub creator: Option<String>,
    /// `created_at` tag, 39000 only: creation time in Unix seconds.
    pub created_at: Option<u64>,
}

pub fn identity<'a>(tags: impl IntoIterator<Item = &'a [String]>) -> ChannelIdentity {
    let mut types = Vec::new();
    let mut creator = None;
    let mut created = None;
    for tag in tags {
        match tag {
            [name, value, ..] if name == "t" => types.push(value.clone()),
            [name, value, ..] if name == "P" && creator.is_none() => creator = Some(value.clone()),
            [name, value, ..] if name == "created_at" && created.is_none() => {
                created = Some(value.clone())
            }
            _ => {}
        }
    }
    let mut types = types.into_iter();
    let channel_type = types.next().filter(|value| !value.is_empty());
    ChannelIdentity {
        channel_type,
        labels: types.filter(|label| valid_label(label)).collect(),
        creator: creator.filter(|key| {
            key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }),
        // Up to 16 digits without a leading zero, and a safe integer.
        created_at: created
            .filter(|value| {
                (1..=16).contains(&value.len())
                    && value.bytes().all(|b| b.is_ascii_digit())
                    && (value == "0" || !value.starts_with('0'))
            })
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|at| *at < 1 << 53),
    }
}

/// The NIP-CL oldest-channel rule: lowest creation time, then lowest id;
/// a channel without a creation time ranks after every one with one.
pub fn compare_age(a: (&str, Option<u64>), b: (&str, Option<u64>)) -> Ordering {
    match (a.1, b.1) {
        (Some(x), Some(y)) if x != y => x.cmp(&y),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        _ => a.0.cmp(b.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn labels_are_short_lowercase_and_never_a_type() {
        assert!(valid_label("for-you"));
        assert!(valid_label("team:core.v2"));
        assert!(!valid_label(""));
        assert!(!valid_label("Caps"));
        assert!(!valid_label("stream"));
        assert!(!valid_label(&"a".repeat(65)));
        assert_eq!(
            label_tags(&strings(&["b", "a", "b"])).unwrap(),
            [["t", "b"], ["t", "a"]]
        );
        assert_eq!(label_tags(&strings(&["dm"])).unwrap_err(), INVALID);
        let nine: Vec<String> = (0..9).map(|i| format!("l{i}")).collect();
        assert_eq!(
            label_tags(&nine).unwrap_err(),
            "Use at most 8 channel labels."
        );
        assert!(same_labels(
            &strings(&["a", "b"]),
            &strings(&["b", "a", "a"])
        ));
        assert!(!same_labels(&strings(&["a"]), &strings(&["a", "b"])));
    }

    #[test]
    fn identity_reads_type_labels_creator_and_time() {
        let creator = "c".repeat(64);
        let tags = [
            strings(&["d", "x"]),
            strings(&["t", "stream"]),
            strings(&["t", "workspace"]),
            strings(&["t", "Bad"]),
            strings(&["P", &creator]),
            strings(&["created_at", "100"]),
        ];
        let read = identity(tags.iter().map(Vec::as_slice));
        assert_eq!(
            read,
            ChannelIdentity {
                channel_type: Some("stream".into()),
                labels: strings(&["workspace"]),
                creator: Some(creator),
                created_at: Some(100),
            }
        );
        let odd = [strings(&["P", "nothex"]), strings(&["created_at", "007"])];
        let read = identity(odd.iter().map(Vec::as_slice));
        assert_eq!((read.creator, read.created_at), (None, None));
    }

    #[test]
    fn the_oldest_channel_comes_first() {
        let mut channels = vec![("b", None), ("c", Some(5)), ("a", Some(5)), ("d", Some(1))];
        channels.sort_by(|a, b| compare_age(*a, *b));
        assert_eq!(
            channels,
            [("d", Some(1)), ("a", Some(5)), ("c", Some(5)), ("b", None)]
        );
    }
}
