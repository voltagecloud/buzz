//! NIP-ER reminders (kind 30300), sealed and opened by buzz-desktop's
//! guard (`relay::reminders`), as buzz-app's `relay_sign_reminder` and
//! `relay_decode_reminders` do. The key only ever encrypts reminder
//! intent to itself and opens the viewer's own reminders; this is not a
//! general NIP-44 or signing primitive.

use super::{Identity, IdentityError};
use buzz_desktop::relay::reminders;
use nostr::prelude::{Event, Kind, SignEvent, Tag, Timestamp, UnsignedEvent};
use serde_json::Value;

pub use reminders::ReminderIntent;

/// One of the viewer's reminders, opened.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedReminder {
    pub event_id: String,
    pub content: Value,
}

fn invalid() -> IdentityError {
    IdentityError::Signing("Invalid reminder".into())
}

impl Identity {
    /// Validates `intent` at `now` (seconds), encrypts it to this key and
    /// signs it, then admits the result as the desktop host does before
    /// publishing: the tags must select the content's status.
    pub fn seal_reminder(&self, intent: ReminderIntent, now: u64) -> Result<Event, IdentityError> {
        let secret = self.secret_bytes();
        let prepared = reminders::prepare(intent, now).map_err(IdentityError::Signing)?;
        let template = prepared.seal(&secret).map_err(IdentityError::Signing)?;
        let tags = template
            .tags
            .into_iter()
            .map(|tag| Tag::parse(tag).map_err(|_| invalid()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut unsigned = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::from_secs(template.created_at),
            Kind::from_u16(template.kind),
            tags,
            template.content,
        );
        unsigned.ensure_id();
        if unsigned.as_json().len() > crate::MAX_EVENT_BYTES {
            return Err(IdentityError::TooLarge);
        }
        let event = self
            .keys
            .sign_event(unsigned)
            .map_err(|error| IdentityError::Signing(error.to_string()))?;
        let raw = serde_json::to_value(&event).map_err(|_| invalid())?;
        let tag = reminders::admission_tag(&raw).map_err(IdentityError::Signing)?;
        reminders::admit(&secret, &self.pubkey_hex(), &raw, tag).map_err(IdentityError::Signing)?;
        Ok(event)
    }

    /// Opens the viewer's own reminders. Records this client could not
    /// save again are skipped, not fatal to the list; a batch over 256
    /// events or 4 MiB is refused.
    pub fn decode_reminders(
        &self,
        events: &[Event],
    ) -> Result<Vec<DecodedReminder>, IdentityError> {
        let raw: Vec<Value> = events
            .iter()
            .map(|event| serde_json::to_value(event).map_err(|_| invalid()))
            .collect::<Result<_, _>>()?;
        reminders::decode_capacity(&raw).map_err(IdentityError::Signing)?;
        let secret = self.secret_bytes();
        let decoded = reminders::decode(&secret, &self.pubkey_hex(), &raw);
        Ok(decoded
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some(DecodedReminder {
                    event_id: entry.get("eventId")?.as_str()?.to_owned(),
                    content: entry.get("content")?.clone(),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn intent(
        created_at: u64,
        not_before: Option<u64>,
        expiration: Option<u64>,
        status: &str,
    ) -> ReminderIntent {
        serde_json::from_value(json!({
            "d": "0123456789abcdef0123456789abcdef",
            "createdAt": created_at,
            "notBefore": not_before,
            "expiration": expiration,
            "content": {
                "target": {
                    "eventId": "a".repeat(64),
                    "channelId": "c1",
                    "preview": "hello",
                    "authorPubkey": "b".repeat(64),
                },
                "status": status,
            },
        }))
        .unwrap()
    }

    #[test]
    fn a_sealed_reminder_opens_only_for_its_owner() {
        let identity = Identity::ephemeral();
        let now = 1_800_000_000;
        let event = identity
            .seal_reminder(intent(now, Some(now + 60), None, "pending"), now)
            .unwrap();
        assert_eq!(event.kind.as_u16(), 30300);
        assert!(!event.content.contains("hello"));
        let decoded = identity
            .decode_reminders(std::slice::from_ref(&event))
            .unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].content["status"], "pending");
        assert!(
            Identity::ephemeral()
                .decode_reminders(&[event])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_guard_refuses_stale_times_and_status_mismatches() {
        let identity = Identity::ephemeral();
        let now = 1_800_000_000;
        assert!(
            identity
                .seal_reminder(intent(now - 3600, Some(now), None, "pending"), now)
                .is_err()
        );
        assert!(
            identity
                .seal_reminder(intent(now, Some(now), None, "done"), now)
                .is_err()
        );
        let closed = now + 45 * 86_400;
        assert!(
            identity
                .seal_reminder(intent(now, None, Some(closed), "done"), now)
                .is_ok()
        );
        assert!(
            identity
                .seal_reminder(intent(now, None, Some(now + 86_400), "done"), now)
                .is_err()
        );
    }
}
