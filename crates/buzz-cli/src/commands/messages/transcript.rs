//! The recorder's narrow ordinary-message path: literal text, stable identity,
//! flat replies, and no mention, emoji, or channel-default expansion.
use nostr::{Event, Tag, Timestamp};
use serde::Deserialize;
use uuid::Uuid;

use super::SendMessageParams;
use crate::client::BuzzClient;
use crate::error::CliError;
use crate::validate::parse_event_id;

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Developer,
    Assistant,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    session: String,
    entry: String,
    role: Role,
    part: u16,
    parts: u16,
    sequence: u64,
    created_at: u64,
    root: Option<String>,
}

pub(super) fn prepare(
    client: &BuzzClient,
    params: &SendMessageParams,
    channel: Uuid,
    json: &str,
) -> Result<Event, CliError> {
    if json.len() > 2048
        || params.kind.is_some_and(|kind| kind != 9)
        || params.reply_to.is_some()
        || params.broadcast
        || !params.files.is_empty()
        || !params.mentions.is_empty()
    {
        return Err(CliError::Usage("invalid transcript options".into()));
    }
    let metadata: Metadata = serde_json::from_str(json)
        .map_err(|_| CliError::Usage("invalid transcript metadata".into()))?;
    for value in [&metadata.session, &metadata.entry] {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(CliError::Usage("invalid transcript identity".into()));
        }
    }
    if metadata.part == 0
        || metadata.part > metadata.parts
        || metadata.parts > 4096
        || metadata.sequence == 0
        || metadata.created_at > Timestamp::now().as_secs()
    {
        return Err(CliError::Usage(
            "invalid transcript position or timestamp".into(),
        ));
    }
    let thread = metadata
        .root
        .as_deref()
        .map(parse_event_id)
        .transpose()?
        .map(|root| buzz_sdk::ThreadRef {
            root_event_id: root,
            parent_event_id: root,
        });
    let role = match metadata.role {
        Role::Developer => "developer",
        Role::Assistant => "assistant",
    };
    let tags = [
        vec!["t".to_owned(), format!("paseo:{role}")],
        vec![
            "paseo".to_owned(),
            metadata.session,
            metadata.entry,
            metadata.part.to_string(),
            metadata.parts.to_string(),
            metadata.sequence.to_string(),
        ],
    ]
    .into_iter()
    .map(Tag::parse)
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| CliError::Usage("invalid transcript tags".into()))?;
    // Do not reuse the normal text expansion path: copied @names, nostr URIs,
    // emoji shortcodes, and channel defaults cannot become live notifications.
    let builder = buzz_sdk::build_message(
        channel,
        &params.content,
        thread.as_ref(),
        &[],
        false,
        &[],
        &[],
    )
    .map_err(|error| CliError::Other(format!("build transcript message: {error}")))?
    .tags(tags)
    .custom_created_at(Timestamp::from_secs(metadata.created_at));
    client.sign_event(builder)
}
