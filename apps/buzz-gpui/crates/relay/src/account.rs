//! The viewer's own profile and membership: the kind 0 a profile save
//! publishes (`docs/port/relay.md` §5f.5) and the NIP-43 leave request
//! (§1.8). Pure builders and response mapping; I/O lives in the session.

use crate::event::{Verified, kinds, newest};
use crate::http::HttpError;
use crate::messages::EventTemplate;
use crate::outbox::{PublishError, accept_publish};
use crate::socket::{Filter, filter};
use nostr::prelude::EventId;
use serde_json::{Map, Value, json};

pub const NAME_LIMIT: usize = 100;
pub const ABOUT_LIMIT: usize = 500;
pub const PICTURE_LIMIT: usize = 2048;
const CONTENT_LIMIT: usize = 16_000;
/// NIP-43 leave request.
pub const LEAVE_KIND: u16 = kinds::LEAVE_COMMUNITY;

/// The fields the profile editor owns. Serialized as buzz-app's
/// `PersonalProfile` (`{name, picture, about?}`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileEdit {
    pub name: String,
    /// Empty, or an HTTPS URL without credentials.
    pub picture: String,
    #[serde(default)]
    pub about: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("Enter a display name.")]
    NameMissing,
    #[error("Keep the display name to 100 characters.")]
    NameTooLong,
    #[error("Shorten the description to 500 characters before saving.")]
    AboutTooLong,
    #[error(
        "Use an HTTPS image URL without credentials (up to 2,048 characters), or remove the avatar."
    )]
    Picture,
    #[error("Your profile is too large to save.")]
    TooLarge,
}

/// Checks a picture as the editor saves it (`avatarPictureError`).
pub fn picture_error(picture: &str) -> Option<ProfileError> {
    if picture.is_empty() {
        return None;
    }
    let ok = picture.len() <= PICTURE_LIMIT
        && url::Url::parse(picture).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some_and(|host| !host.is_empty())
                && url.username().is_empty()
                && url.password().is_none()
        });
    (!ok).then_some(ProfileError::Picture)
}

/// Validates an edit (`native-api.ts:184-224`), returning it trimmed.
pub fn validate(edit: &ProfileEdit) -> Result<ProfileEdit, ProfileError> {
    let name = edit.name.trim();
    if name.is_empty() {
        return Err(ProfileError::NameMissing);
    }
    if name.encode_utf16().count() > NAME_LIMIT {
        return Err(ProfileError::NameTooLong);
    }
    let about = edit.about.trim();
    if about.encode_utf16().count() > ABOUT_LIMIT {
        return Err(ProfileError::AboutTooLong);
    }
    let picture = edit.picture.trim();
    if let Some(error) = picture_error(picture) {
        return Err(error);
    }
    Ok(ProfileEdit {
        name: name.into(),
        picture: picture.into(),
        about: about.into(),
    })
}

/// The read of the viewer's own kind 0 (`ProfileButton.tsx:147`).
pub fn own_profile_filter(viewer: &str) -> Filter {
    filter(json!({"kinds": [kinds::PROFILE], "authors": [viewer], "limit": 5}))
}

/// The newest of the viewer's kind 0 events.
pub fn newest_profile<'a>(events: &'a [Verified], viewer: &str) -> Option<&'a Verified> {
    newest(
        events
            .iter()
            .filter(|event| event.kind_u16() == kinds::PROFILE && event.pubkey_hex() == viewer),
    )
}

/// The editor's fields in a kind 0's content (`inspectProfile`): the
/// display name, else the name. Content that is not an object reads as
/// empty fields.
pub fn profile_fields(content: &str) -> ProfileEdit {
    let fields = match serde_json::from_str::<Value>(content) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let text = |key: &str| match fields.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    };
    let string = |key: &str| match fields.get(key) {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    };
    ProfileEdit {
        name: text("display_name")
            .or_else(|| text("name"))
            .unwrap_or_default(),
        picture: string("picture"),
        about: string("about"),
    }
}

/// The kind 0 that saves `edit` over `existing` (the current kind 0's
/// content). Unknown fields survive; `name` and `display_name` both take
/// the display name. Dated after `previous` so it replaces it.
pub fn profile_template(
    edit: &ProfileEdit,
    existing: Option<&str>,
    previous: Option<u64>,
    now: u64,
) -> Result<EventTemplate, ProfileError> {
    let edit = validate(edit)?;
    let mut fields: Map<String, Value> = existing
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    fields.insert("name".into(), Value::String(edit.name.clone()));
    fields.insert("display_name".into(), Value::String(edit.name));
    fields.insert("picture".into(), Value::String(edit.picture));
    fields.insert("about".into(), Value::String(edit.about));
    let content = Value::Object(fields).to_string();
    if content.len() > CONTENT_LIMIT {
        return Err(ProfileError::TooLarge);
    }
    Ok(EventTemplate {
        kind: kinds::PROFILE,
        created_at: now.max(previous.map_or(0, |at| at + 1)),
        tags: Vec::new(),
        content,
    })
}

/// The leave request: exactly this shape, or the relay refuses it.
pub fn leave_template(now: u64) -> EventTemplate {
    EventTemplate {
        kind: LEAVE_KIND,
        created_at: now,
        tags: vec![vec!["-".into()]],
        content: String::new(),
    }
}

/// How a leave ended, when it let the app drop the community.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaveOutcome {
    /// The relay removed the membership.
    Left,
    /// The viewer was not a member (or membership is off): drop the
    /// community and its device state anyway.
    AlreadyAbsent,
    /// The viewer is banned: drop the community but keep device state.
    Revoked,
}

/// The leave error when the viewer owns the relay.
pub const OWNER_CANNOT_LEAVE: &str = "You own this community, so you can't leave it.";

/// Maps a `POST /events` answer to a leave request. `Err` keeps the
/// membership so the user can retry.
pub fn leave_outcome(
    result: Result<Vec<u8>, HttpError>,
    id: &EventId,
) -> Result<LeaveOutcome, String> {
    let refusal = match &result {
        Err(HttpError::Status { message, .. }) => Some(message.clone()),
        _ => None,
    };
    let message = match accept_publish(result, id) {
        Ok(_) => return Ok(LeaveOutcome::Left),
        Err(PublishError::Rejected(message)) => refusal.unwrap_or(message),
        Err(PublishError::Uncertain(message)) => return Err(message),
    };
    match message.as_str() {
        // The relay's HTTP API turns non-members away before the leave
        // handler, with the second code; buzz-app only knows the first.
        "invalid: you are not a relay member"
        | "invalid: relay membership is not enabled"
        | "relay_membership_required" => Ok(LeaveOutcome::AlreadyAbsent),
        "blocked: you are banned from this community" => Ok(LeaveOutcome::Revoked),
        "invalid: relay owner cannot leave" => Err(OWNER_CANNOT_LEAVE.into()),
        "community writes are temporarily unavailable" => {
            Err("This community isn't accepting changes right now. Try again later.".into())
        }
        _ => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(name: &str, picture: &str, about: &str) -> ProfileEdit {
        ProfileEdit {
            name: name.into(),
            picture: picture.into(),
            about: about.into(),
        }
    }

    #[test]
    fn keeps_unknown_fields_and_mirrors_the_name() {
        let existing = r#"{"name":"old","nip05":"a@b.c","lud16":"x@y.z","about":"was"}"#;
        let template =
            profile_template(&edit(" Alice ", "", " hi "), Some(existing), Some(9), 5).unwrap();
        assert_eq!(template.kind, 0);
        assert!(template.tags.is_empty());
        assert_eq!(template.created_at, 10);
        let fields: Value = serde_json::from_str(&template.content).unwrap();
        assert_eq!(fields["name"], "Alice");
        assert_eq!(fields["display_name"], "Alice");
        assert_eq!(fields["about"], "hi");
        assert_eq!(fields["picture"], "");
        assert_eq!(fields["nip05"], "a@b.c");
        assert_eq!(fields["lud16"], "x@y.z");
        // Content that is not an object starts over.
        let fresh = profile_template(&edit("A", "", ""), Some("[1]"), None, 5).unwrap();
        let fields: Value = serde_json::from_str(&fresh.content).unwrap();
        assert_eq!(fields.as_object().unwrap().len(), 4);
        assert_eq!(fresh.created_at, 5);
    }

    #[test]
    fn reads_the_editor_fields_like_inspect_profile() {
        assert_eq!(
            profile_fields(
                r#"{"name":"al","display_name":"Alice","picture":"https://x/a.png","about":"hi"}"#
            ),
            edit("Alice", "https://x/a.png", "hi")
        );
        assert_eq!(profile_fields(r#"{"name":"al"}"#), edit("al", "", ""));
        assert_eq!(
            profile_fields(r#"{"display_name":null,"name":"al","picture":7}"#),
            edit("al", "", "")
        );
        assert_eq!(profile_fields("[1]"), ProfileEdit::default());
        assert_eq!(profile_fields("not json"), ProfileEdit::default());
    }

    #[test]
    fn validates_like_the_reference() {
        assert_eq!(
            validate(&edit("  ", "", "")),
            Err(ProfileError::NameMissing)
        );
        assert_eq!(
            validate(&edit(&"n".repeat(101), "", "")),
            Err(ProfileError::NameTooLong)
        );
        assert_eq!(
            validate(&edit("a", "", &"x".repeat(501))),
            Err(ProfileError::AboutTooLong)
        );
        for bad in [
            "http://x.example/a.png",
            "https://u:p@x.example/a.png",
            "data:image/png;base64,AAAA",
            "not a url",
        ] {
            assert_eq!(validate(&edit("a", bad, "")), Err(ProfileError::Picture));
        }
        assert!(validate(&edit("a", "https://x.example/a.png", "")).is_ok());
    }

    #[test]
    fn maps_leave_refusals() {
        let id = EventId::from_byte_array([0; 32]);
        let refused = |message: &str| {
            leave_outcome(
                Err(HttpError::Status {
                    status: 400,
                    message: message.into(),
                    retry_after: None,
                    not_sent: false,
                }),
                &id,
            )
        };
        assert_eq!(
            refused("invalid: you are not a relay member"),
            Ok(LeaveOutcome::AlreadyAbsent)
        );
        assert_eq!(
            refused("invalid: relay membership is not enabled"),
            Ok(LeaveOutcome::AlreadyAbsent)
        );
        assert_eq!(
            refused("relay_membership_required"),
            Ok(LeaveOutcome::AlreadyAbsent)
        );
        assert_eq!(
            refused("blocked: you are banned from this community"),
            Ok(LeaveOutcome::Revoked)
        );
        assert!(refused("invalid: relay owner cannot leave").is_err());
        assert!(refused("something else").is_err());
        let receipt = |accepted: bool, message: &str| {
            serde_json::json!({"event_id": id.to_hex(), "accepted": accepted, "message": message})
                .to_string()
                .into_bytes()
        };
        assert_eq!(
            leave_outcome(Ok(receipt(true, "")), &id),
            Ok(LeaveOutcome::Left)
        );
        assert_eq!(
            leave_outcome(
                Ok(receipt(false, "invalid: you are not a relay member")),
                &id
            ),
            Ok(LeaveOutcome::AlreadyAbsent)
        );
        assert_eq!(leave_template(7).tags, vec![vec!["-".to_string()]]);
    }
}
