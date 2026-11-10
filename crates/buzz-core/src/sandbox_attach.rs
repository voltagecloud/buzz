//! Coordinates whose first accepted event permanently consumes an attachment.

use nostr::Event;
use uuid::Uuid;

/// Parse a canonical terminal or snapshot acceptance coordinate. Requests,
/// approvals and end records retain ordinary replacement/deletion semantics.
pub fn parse_acceptance_coordinate(value: &str) -> Option<(&str, Uuid)> {
    let mut parts = value.split(':');
    let mode = parts.next()?;
    if !matches!(mode, "terminal" | "snapshot") {
        return None;
    }
    let text = parts.next()?;
    let session = Uuid::parse_str(text).ok()?;
    if session.to_string() != text || parts.next()? != "accepted" || parts.next().is_some() {
        return None;
    }
    Some((mode, session))
}

/// Identify an unambiguous kind30624 acceptance coordinate. This classifies a
/// stored envelope; the relay separately verifies its signed audit metadata.
pub fn acceptance_coordinate(event: &Event) -> Option<(&str, Uuid)> {
    if u32::from(event.kind.as_u16()) != crate::kind::KIND_SANDBOX_ATTACH {
        return None;
    }
    let mut tags = event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().is_some_and(|name| name == "d"));
    let [_, value] = tags.next()? else {
        return None;
    };
    if tags.next().is_some() {
        return None;
    }
    parse_acceptance_coordinate(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_canonical_acceptance_coordinates_are_burns() {
        let session: Uuid = "abcdefab-1234-4321-8765-abcdefabcdef".parse().unwrap();
        for mode in ["terminal", "snapshot"] {
            assert_eq!(
                parse_acceptance_coordinate(&format!("{mode}:{session}:accepted")),
                Some((mode, session))
            );
            for value in [
                format!("{mode}:{session}:grant"),
                format!("{mode}:{session}:request"),
                format!("{mode}:{session}:ended"),
                format!("{mode}:{session}:accepted:extra"),
                format!("{mode}:{}:accepted", session.to_string().to_uppercase()),
            ] {
                assert!(parse_acceptance_coordinate(&value).is_none(), "{value}");
            }
        }
    }
}
