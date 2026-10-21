//! Templates for the events a user writes: messages, thread replies,
//! sent-from-thread copies, edits, deletions, reactions, reports and the
//! open-DM command. Pure: no I/O and no clock reads; callers pass `now`.
//!
//! Ports `messages.ts`, the write side of `emoji.ts` and `attachments.ts`,
//! `message-order.ts` and the 41010 route of `channel_writes.rs`. See
//! `docs/port/relay.md` §5c.1, §5d, §5e.1, §5k.4 and §5n.4.
//!
//! Builders that go through the outbox return a [`Draft`]; the outbox picks
//! `created_at` (§6.2) and appends the `client-id` and `ms` tags. Direct
//! publications (reports, 41010) return a dated [`EventTemplate`].
//!
//! Session-level gates stay with the caller: "can participate in this
//! channel", "every recipient is a current member" (`validateMentions`) and
//! "the emoji catalog is ready" (passed in as [`EmojiCatalog`]).

use crate::event::{first_tag, is_hex64, kinds};
use crate::fold::{message_author, thread_reference};
use crate::origin::RelayOrigin;
use crate::outbox::Delivery;
use nostr::prelude::{Event, EventId, PublicKey};
use regex::Regex;

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

pub type Tag = Vec<String>;

/// An event without its time. The outbox dates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub kind: u16,
    pub content: String,
    pub tags: Vec<Tag>,
}

impl Draft {
    pub fn at(self, created_at: u64) -> EventTemplate {
        EventTemplate {
            kind: self.kind,
            created_at,
            tags: self.tags,
            content: self.content,
        }
    }
}

/// Everything [`buzz_identity::Identity::template`] needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventTemplate {
    pub kind: u16,
    pub created_at: u64,
    pub tags: Vec<Tag>,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    #[error("Message is empty")]
    Empty,
    /// The catalog is still loading and the text names a `:code:`. The
    /// draft is kept; the caller should start loading the catalog.
    #[error("Community emoji is loading. Your draft is kept; try sending again shortly.")]
    EmojiLoading,
    #[error("Community emoji unavailable. Retry message preparation or open the emoji picker. {0}")]
    EmojiUnavailable(String),
    #[error("{0}")]
    Invalid(&'static str),
}

use MessageError::Invalid;

fn tag(items: &[&str]) -> Tag {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn tags_of<'a>(event: &'a Event) -> impl Iterator<Item = &'a [String]> + 'a {
    event.tags.iter().map(|tag| tag.as_slice())
}

/// Whether `source`, where attachments are read from, is `original` or
/// the author's edit of it that stays in `channel_id`.
fn is_version_of(author: &Author<'_>, source: &Event, original: &Event, channel_id: &str) -> bool {
    let original_id = original.id.to_hex();
    source.id == original.id
        || (source.kind.as_u16() == kinds::EDIT
            && author.owns(source)
            && tags_of(source).any(|tag| {
                tag.first().map(String::as_str) == Some("e")
                    && tag.get(1).map(String::as_str) == Some(original_id.as_str())
            })
            && !tags_of(source).any(|tag| {
                tag.first().map(String::as_str) == Some("h")
                    && tag.get(1).map(String::as_str) != Some(channel_id)
            }))
}

/// `content.trim()`, refusing empty text (`messages.ts:51-55`).
fn text(content: &str) -> Result<String, MessageError> {
    let value = content.trim();
    if value.is_empty() {
        return Err(MessageError::Empty);
    }
    Ok(value.to_owned())
}

// ---------------------------------------------------------------------------
// Authorship and thread references

/// Who is writing, for ownership checks.
#[derive(Clone, Copy, Debug)]
pub struct Author<'a> {
    pub viewer: &'a str,
    /// The relay's NIP-11 `self`. Never inferred from a displayed row.
    pub signing_authority: Option<&'a str>,
}

impl Author<'_> {
    fn owns(&self, event: &Event) -> bool {
        self.displayed_author(event) == self.viewer
    }

    fn displayed_author(&self, event: &Event) -> String {
        let authority = self
            .signing_authority
            .and_then(|key| PublicKey::from_hex(key).ok());
        message_author(event, authority.as_ref()).to_hex()
    }
}

// ---------------------------------------------------------------------------
// Custom emoji (`emoji.ts`, `emoji-directory.ts:292-311`, §5k.4)

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomEmoji {
    pub shortcode: String,
    pub url: String,
}

/// The community palette as the session holds it.
#[derive(Clone, Copy, Debug)]
pub enum EmojiCatalog<'a> {
    Ready(&'a [CustomEmoji]),
    Loading,
    Unavailable(&'a str),
}

/// Trim, strip surrounding colons, lowercase; `[a-z0-9_-]{1,64}` or nothing.
pub fn normalize_shortcode(value: &str) -> Option<String> {
    let code = value.trim().trim_matches(':').to_lowercase();
    let valid = (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    valid.then_some(code)
}

/// Reaction text stays small, while a valid custom token gets its two
/// delimiters.
pub fn valid_reaction_content(value: &str) -> bool {
    if value.chars().count() <= 64 {
        return true;
    }
    value
        .strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .is_some_and(|code| {
            (1..=64).contains(&code.len())
                && code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s<>"`]+"#).expect("link pattern"));

/// Text outside `http(s)` links, which never carry emoji substitutions.
fn text_parts(content: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for link in LINK.find_iter(content) {
        parts.push(&content[start..link.start()]);
        start = link.end();
    }
    parts.push(&content[start..]);
    parts
}

/// Every `:code:` candidate in `part`: (start, end, code).
fn shortcodes(part: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    let bytes = part.as_bytes();
    (0..bytes.len()).filter_map(move |start| {
        if bytes[start] != b':' {
            return None;
        }
        let run = bytes[start + 1..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_' || **b == b'-')
            .count();
        let close = start + 1 + run;
        ((1..=64).contains(&run) && bytes.get(close) == Some(&b':'))
            .then(|| (start, close + 1, &part[start + 1..close]))
    })
}

/// Lowercased codes the text names outside links, deduplicated.
pub fn referenced_emoji(content: &str) -> Vec<String> {
    let mut seen = Vec::<String>::new();
    for part in text_parts(content) {
        for (_, _, code) in shortcodes(part) {
            let code = code.to_ascii_lowercase();
            if !seen.contains(&code) {
                seen.push(code);
            }
        }
    }
    seen
}

/// `["emoji", code, url]` for each catalog code the typed text uses. Unknown
/// codes stay plain text. A text that names any code needs a ready catalog.
pub fn emoji_tags(content: &str, catalog: EmojiCatalog<'_>) -> Result<Vec<Tag>, MessageError> {
    if referenced_emoji(content).is_empty() {
        return Ok(Vec::new());
    }
    let entries = match catalog {
        EmojiCatalog::Ready(entries) => entries,
        EmojiCatalog::Loading => return Err(MessageError::EmojiLoading),
        EmojiCatalog::Unavailable(error) => {
            return Err(MessageError::EmojiUnavailable(error.to_owned()));
        }
    };
    let mut used: Vec<(&str, &str)> = Vec::new();
    for part in text_parts(content) {
        let mut end = 0;
        for (start, stop, code) in shortcodes(part) {
            if start < end {
                continue;
            }
            let code = code.to_ascii_lowercase();
            let Some(emoji) = entries.iter().find(|entry| entry.shortcode == code) else {
                continue;
            };
            end = stop;
            match used.iter_mut().find(|(code, _)| *code == emoji.shortcode) {
                Some(slot) => slot.1 = &emoji.url,
                None => used.push((&emoji.shortcode, &emoji.url)),
            }
        }
    }
    Ok(used
        .into_iter()
        .map(|(code, url)| tag(&["emoji", code, url]))
        .collect())
}

// ---------------------------------------------------------------------------
// Attachments (`attachments.ts:55-106,284-334`, §5n.3-5n.4)

const MIB: u64 = 1024 * 1024;

/// A completed upload, as the media module returns it.
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub url: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    /// Seconds, for voice notes. The waveform is never written: the relay
    /// refuses the `waveform` imeta key (§9.3 #2).
    pub voice_duration: Option<f64>,
}

/// Client-side per-type cap (`attachment-limits.ts`).
pub fn media_byte_limit(mime: &str) -> u64 {
    if mime == "image/gif" {
        10 * MIB
    } else if mime.starts_with("image/") {
        50 * MIB
    } else if mime.starts_with("video/") {
        500 * MIB
    } else {
        100 * MIB
    }
}

const INVALID_UPLOAD: MessageError =
    Invalid("The server returned an invalid upload result. Retry this file.");

/// Re-checks an upload descriptor against the community origin: a MIME
/// token, a hex digest, a size within the type's cap and a same-origin
/// `/media/<sha256>[.ext]` URL. Returns the resolved URL and lowercase type.
pub(crate) fn validate_upload(
    item: &Attachment,
    origin: &RelayOrigin,
) -> Result<(String, String), MessageError> {
    static MIME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^[a-z0-9.+-]+/[a-z0-9.+-]+$").expect("mime pattern"));
    let mime = item.mime.to_ascii_lowercase();
    if !MIME.is_match(&item.mime)
        || !is_hex64(&item.sha256)
        || item.size < 1
        || item.size > media_byte_limit(&mime)
    {
        return Err(INVALID_UPLOAD);
    }
    let base = url::Url::parse(&origin.http()).map_err(|_| INVALID_UPLOAD)?;
    let url = base.join(&item.url).map_err(|_| INVALID_UPLOAD)?;
    let path = url.path();
    let extension = path
        .strip_prefix("/media/")
        .and_then(|rest| rest.strip_prefix(item.sha256.as_str()));
    let path_ok = match extension {
        Some("") => true,
        Some(ext) => ext.strip_prefix('.').is_some_and(|ext| {
            (1..=8).contains(&ext.len())
                && ext
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        }),
        None => false,
    };
    if url.origin() != base.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !path_ok
    {
        return Err(INVALID_UPLOAD);
    }
    Ok((url.to_string(), mime))
}

fn unsafe_name_char(c: char) -> bool {
    let code = c as u32;
    code <= 0x1f
        || (0x7f..=0x9f).contains(&code)
        || code == 0x200e
        || code == 0x200f
        || (0x202a..=0x202e).contains(&code)
        || (0x2066..=0x2069).contains(&code)
}

/// A basename capped at 255 UTF-8 bytes; `File` when nothing is left.
fn attachment_filename(name: &str) -> String {
    let mut clean = String::new();
    for c in name.chars() {
        let c = if unsafe_name_char(c) || c == '/' || c == '\\' {
            ' '
        } else {
            c
        };
        if clean.len() + c.len_utf8() > 255 {
            break;
        }
        clean.push(c);
    }
    match clean.trim() {
        "" => "File".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

fn attachment_markdown(name: &str, url: &str, mime: &str) -> String {
    let mut label = String::new();
    for c in name.chars() {
        let c = if (c as u32) < 32 { ' ' } else { c };
        if "\\`*_{}[]()!<>&".contains(c) {
            label.push('\\');
        }
        label.push(c);
    }
    let inline = matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "video/mp4"
    );
    format!("{}[{label}](<{url}>)", if inline { "!" } else { "" })
}

/// The message body with one markdown link per attachment, and one `imeta`
/// tag per attachment in the relay's field order.
pub fn attachment_message(
    content: &str,
    attachments: &[Attachment],
    origin: &RelayOrigin,
) -> Result<(String, Vec<Tag>), MessageError> {
    let mut tags = Vec::new();
    let mut parts = vec![content.trim().to_owned()];
    for item in attachments {
        let (url, mime) = validate_upload(item, origin)?;
        let name = attachment_filename(&item.name);
        let mut imeta = vec![
            "imeta".to_owned(),
            format!("url {url}"),
            format!("m {mime}"),
            format!("size {}", item.size),
            format!("x {}", item.sha256),
            format!("filename {name}"),
        ];
        if let Some(duration) = item.voice_duration {
            if !duration.is_finite() || duration <= 0.0 || duration > 300.1 {
                return Err(INVALID_UPLOAD);
            }
            imeta.push(format!("duration {duration}"));
        }
        tags.push(imeta);
        parts.push(attachment_markdown(&name, &url, &mime));
    }
    let body = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok((body, tags))
}

// ---------------------------------------------------------------------------
// Messages and replies (§5d.2-5d.4)

/// `["model", agentPubkey, model]`: what a mentioned agent's owner asks
/// of its next run. Only the owner is honoured.
pub const MODEL_TAG: &str = "model";
/// `["effort", agentPubkey, level]`, as [`MODEL_TAG`].
pub const EFFORT_TAG: &str = "effort";

/// The model and effort picked for a mentioned agent's next run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunPick {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Chosen mention chips. Recipients are notified (`p`) and must be current
/// channel members, which the caller checks. References only bind display
/// names (`mention`). Picks ask mentioned agents for a model or effort.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mentions {
    pub recipients: Vec<String>,
    pub references: Vec<String>,
    /// The model and effort picked for each mentioned agent, by pubkey.
    pub picks: BTreeMap<String, RunPick>,
}

static PICKED_MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.:\[\]-]{1,64}$").expect("model pattern"));
static PICKED_EFFORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9-]{1,32}$").expect("effort pattern"));

/// One `model` and one `effort` tag per pick; a pick applies only to a
/// mentioned agent.
fn pick_tags(mentions: &Mentions) -> Result<Vec<Tag>, MessageError> {
    let mut tags = Vec::new();
    for (key, pick) in &mentions.picks {
        let valid = |value: &Option<String>, pattern: &Regex| {
            value.as_deref().is_none_or(|value| pattern.is_match(value))
        };
        if !mentions.recipients.contains(key)
            || !valid(&pick.model, &PICKED_MODEL)
            || !valid(&pick.effort, &PICKED_EFFORT)
        {
            return Err(Invalid(
                "A model or effort applies only to a mentioned agent",
            ));
        }
        if let Some(model) = &pick.model {
            tags.push(tag(&[MODEL_TAG, key, model]));
        }
        if let Some(effort) = &pick.effort {
            tags.push(tag(&[EFFORT_TAG, key, effort]));
        }
    }
    Ok(tags)
}

fn unique_keys(
    name: &str,
    pubkeys: &[String],
    error: &'static str,
) -> Result<Vec<Tag>, MessageError> {
    // Count after removing repeats: a forwarded message may repeat a key.
    let mut seen = HashSet::new();
    let unique: Vec<&String> = pubkeys
        .iter()
        .filter(|key| seen.insert(key.as_str()))
        .collect();
    if unique.len() > 32 || unique.iter().any(|key| !is_hex64(key)) {
        return Err(Invalid(error));
    }
    Ok(unique.into_iter().map(|key| tag(&[name, key])).collect())
}

fn recipient_tags(pubkeys: &[String]) -> Result<Vec<Tag>, MessageError> {
    unique_keys("p", pubkeys, "Choose at most 32 valid mention recipients")
}

fn reference_tags(pubkeys: &[String]) -> Result<Vec<Tag>, MessageError> {
    unique_keys(
        "mention",
        pubkeys,
        "Choose at most 32 valid mention references",
    )
}

/// What the composer submits.
#[derive(Clone, Copy, Debug)]
pub struct Compose<'a> {
    pub channel_id: &'a str,
    pub content: &'a str,
    pub mentions: &'a Mentions,
    pub attachments: &'a [Attachment],
    pub emoji: EmojiCatalog<'a>,
    pub origin: &'a RelayOrigin,
}

fn compose(compose: &Compose<'_>, thread: Vec<Tag>) -> Result<Draft, MessageError> {
    if compose.channel_id.is_empty() {
        return Err(Invalid("A channel is required"));
    }
    let (body, imeta) = attachment_message(compose.content, compose.attachments, compose.origin)?;
    let content = text(&body)?;
    let mut tags = vec![tag(&["h", compose.channel_id])];
    tags.extend(thread);
    tags.extend(recipient_tags(&compose.mentions.recipients)?);
    tags.extend(reference_tags(&compose.mentions.references)?);
    tags.extend(pick_tags(compose.mentions)?);
    // Emoji come from the text the user typed, not attachment labels.
    tags.extend(emoji_tags(compose.content, compose.emoji)?);
    tags.extend(imeta);
    Ok(Draft {
        kind: kinds::CHAT,
        content,
        tags,
    })
}

/// A top-level kind 9 (§5d.2). Tag order: `h`, `p`…, `mention`…, `emoji`…,
/// `imeta`….
pub fn message(input: &Compose<'_>) -> Result<Draft, MessageError> {
    compose(input, Vec::new())
}

/// A thread reply (§5c.1, §5d.4). A direct reply to the root carries only
/// `["e", root, "", "reply"]`; a nested one adds the `root` marker first.
pub fn reply(input: &Compose<'_>, root_id: &str, parent_id: &str) -> Result<Draft, MessageError> {
    if input.channel_id.is_empty() {
        return Err(Invalid("A channel is required"));
    }
    if !is_hex64(root_id) {
        return Err(Invalid("A valid thread root is required"));
    }
    if !is_hex64(parent_id) {
        return Err(Invalid("A valid reply parent is required"));
    }
    let mut thread = Vec::new();
    if parent_id != root_id {
        thread.push(tag(&["e", root_id, "", "root"]));
    }
    thread.push(tag(&["e", parent_id, "", "reply"]));
    compose(input, thread)
}

// ---------------------------------------------------------------------------
// Sent-from-thread copy (§5d.5)

/// The reconciled thread root, as the mounted thread shows it.
#[derive(Clone, Copy, Debug)]
pub struct ThreadRoot<'a> {
    pub id: &'a str,
    pub channel_id: &'a str,
    pub content: &'a str,
}

/// The user's own reply as the thread view folded it.
#[derive(Clone, Copy, Debug)]
pub struct SentFromThread<'a> {
    /// The retained kind-9 reply event.
    pub reply: &'a Event,
    /// Where the row's media came from: the reply itself, or the latest own
    /// 40003 that carried `imeta`.
    pub attachment_source: &'a Event,
    /// The row's displayed body (the edited source text when edited).
    pub content: &'a str,
    /// Used as the body when the text is empty.
    pub attachment_urls: &'a [String],
    /// The row's `p` recipients followed by its `mention` references.
    pub mentions: &'a [String],
    pub edited: bool,
    pub emoji: &'a [CustomEmoji],
    /// Outbox state of the reply, if it is still in the outbox.
    pub delivery: Option<Delivery>,
    /// For the excerpt; ignored unless it is this reply's root.
    pub root: Option<ThreadRoot<'a>>,
}

static SPOILER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\|\|(?s:.)*?(?:\|\||$)").expect("spoiler pattern"));
static IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\([^)]*\)").expect("image pattern"));
static MARKDOWN_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]*\)").expect("link pattern"));
static BARE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:https?://|buzz://)\S+").expect("url pattern"));
static MARKUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[`*_~>#|]").expect("markup pattern"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("space pattern"));

/// The root's text without spoilers, images, link targets, URLs and
/// markup, at most 64 code points (`messages.ts:134-150`).
pub fn thread_excerpt(content: &str) -> String {
    let text = SPOILER.replace_all(content, " ");
    let text = IMAGE.replace_all(&text, " ");
    let text = MARKDOWN_LINK.replace_all(&text, "$1");
    let text = BARE_URL.replace_all(&text, " ");
    let text = MARKUP.replace_all(&text, " ");
    let text = SPACES.replace_all(&text, " ");
    let chars: Vec<char> = text.trim().chars().collect();
    if chars.len() > 64 {
        let mut cut: String = chars[..63].iter().collect();
        cut.push('…');
        cut
    } else {
        chars.into_iter().collect()
    }
}

/// A new top-level kind 9 copying the user's own thread reply. Recipients
/// become references so nobody is notified twice; an edited reply drops
/// them entirely.
pub fn sent_from_thread(
    author: &Author<'_>,
    row: &SentFromThread<'_>,
) -> Result<Draft, MessageError> {
    let original = row.reply;
    let thread = (original.kind.as_u16() == kinds::CHAT)
        .then(|| thread_reference(original))
        .flatten()
        .ok_or(Invalid(
            "Load a thread reply before sending it to the channel",
        ))?;
    if !author.owns(original) {
        return Err(Invalid("Only your own messages can be sent to the channel"));
    }
    let channel_id = first_tag(original, "h")
        .filter(|channel| !channel.is_empty())
        .ok_or(Invalid(
            "Reload the thread reply before sending it to the channel",
        ))?;
    if row
        .delivery
        .is_some_and(|delivery| !matches!(delivery, Delivery::Accepted | Delivery::Seen))
    {
        return Err(Invalid(
            "Reload the thread reply before sending it to the channel",
        ));
    }
    let source = row.attachment_source;
    if !(author.owns(source) && is_version_of(author, source, original, channel_id)) {
        return Err(Invalid("Reload the message attachments before sending"));
    }
    let label = row
        .root
        .filter(|root| root.id == thread.root_id.to_hex() && root.channel_id == channel_id)
        .map(|root| thread_excerpt(root.content))
        .unwrap_or_default();
    let body = if row.content.trim().is_empty() {
        row.attachment_urls.join("\n")
    } else {
        row.content.to_owned()
    };
    let mut provenance = vec!["buzz:sent-from-thread".to_owned(), thread.root_id.to_hex()];
    if !label.is_empty() {
        provenance.push(label);
    }
    let mut tags = vec![tag(&["h", channel_id]), provenance];
    tags.extend(reference_tags(if row.edited { &[] } else { row.mentions })?);
    tags.extend(
        row.emoji
            .iter()
            .map(|emoji| tag(&["emoji", &emoji.shortcode, &emoji.url])),
    );
    tags.extend(
        tags_of(source)
            .filter(|tag| {
                matches!(
                    tag.first().map(String::as_str),
                    Some("imeta" | "link-preview")
                )
            })
            .map(<[String]>::to_vec),
    );
    Ok(Draft {
        kind: kinds::CHAT,
        content: text(&body)?,
        tags,
    })
}

// ---------------------------------------------------------------------------
// Edit (§5d.7)

/// A kind-40003 edit of the viewer's own message. `attachment_source` is the
/// folded row's media provenance: the original, or the latest own edit that
/// carried `imeta`. Edits cannot change attachments or recipients. The
/// outbox makes successive edits of one target strictly increasing (§9.2 #19).
pub fn edit(
    author: &Author<'_>,
    original: &Event,
    attachment_source: &Event,
    content: &str,
    emoji: EmojiCatalog<'_>,
) -> Result<Draft, MessageError> {
    if !matches!(original.kind.as_u16(), kinds::CHAT | kinds::AGENT_MESSAGE) {
        return Err(Invalid("Load the message before editing it"));
    }
    if !author.owns(original) {
        return Err(Invalid("Only your own messages can be edited"));
    }
    let channel_id = first_tag(original, "h").ok_or(Invalid("Message has no channel"))?;
    let message_id = original.id.to_hex();
    let source = attachment_source;
    let source_ok = is_version_of(author, source, original, channel_id)
        && (source.id == original.id
            || tags_of(source).any(|tag| tag.first().map(String::as_str) == Some("imeta")));
    if !source_ok {
        return Err(Invalid(
            "Reload the message before editing its attachments.",
        ));
    }
    let content = text(content)?;
    let mut tags = vec![tag(&["h", channel_id]), tag(&["e", &message_id])];
    tags.extend(
        tags_of(source)
            .filter(|tag| tag.first().map(String::as_str) == Some("imeta"))
            .map(<[String]>::to_vec),
    );
    tags.extend(emoji_tags(&content, emoji)?);
    Ok(Draft {
        kind: kinds::EDIT,
        content,
        tags,
    })
}

// ---------------------------------------------------------------------------
// Reactions (§5d.9)

/// A kind-7 reaction: `h`, `e`, and for a custom emoji exactly one `emoji`
/// tag. No `p` or `k`.
pub fn react(
    original: &Event,
    content: &str,
    custom: Option<&CustomEmoji>,
    catalog: EmojiCatalog<'_>,
) -> Result<Draft, MessageError> {
    if !matches!(
        original.kind.as_u16(),
        kinds::CHAT | kinds::AGENT_MESSAGE | kinds::DIFF
    ) {
        return Err(Invalid("Load the message before reacting to it"));
    }
    let channel_id = first_tag(original, "h").ok_or(Invalid("Message has no channel"))?;
    let value = text(content)?;
    if !valid_reaction_content(&value) {
        return Err(Invalid("Reaction is too long"));
    }
    let mut tags = vec![tag(&["h", channel_id]), tag(&["e", &original.id.to_hex()])];
    match custom {
        Some(emoji) if value.to_lowercase() == format!(":{}:", emoji.shortcode) => {
            tags.push(tag(&["emoji", &emoji.shortcode, &emoji.url]));
        }
        _ => tags.extend(emoji_tags(&value, catalog)?),
    }
    Ok(Draft {
        kind: kinds::REACTION,
        content: value,
        tags,
    })
}

// ---------------------------------------------------------------------------
// Deletion (§5d.8)

/// Owner authorization, supplied only after the agent's signed profile was
/// verified.
#[derive(Clone, Copy, Debug)]
pub struct AgentOwner<'a> {
    pub agent_id: &'a str,
    pub owner_id: &'a str,
}

/// Kind-5 deletions of the viewer's own messages and reactions, or of an
/// agent's messages by its verified owner. One event per target: the relay
/// refuses a deletion with more than one `e` (§9.3 #1). Each is
/// `[["h", ch], ["e", id], ["k", kind]]`.
pub fn remove<'a>(
    author: &Author<'_>,
    ids: &[String],
    find: impl Fn(&str) -> Option<&'a Event>,
    authorization: Option<AgentOwner<'_>>,
) -> Result<Vec<Draft>, MessageError> {
    let mut seen = HashSet::new();
    let ids: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| seen.insert(*id))
        .collect();
    if ids.is_empty() || ids.len() > 100 {
        return Err(Invalid("Choose between 1 and 100 events to remove"));
    }
    let mut targets = Vec::with_capacity(ids.len());
    for id in &ids {
        let event = find(id)
            .filter(|event| {
                matches!(
                    event.kind.as_u16(),
                    kinds::REACTION | kinds::CHAT | kinds::AGENT_MESSAGE
                )
            })
            .ok_or(Invalid("Load the message or reaction before removing it"))?;
        let allowed = if event.kind.as_u16() == kinds::REACTION {
            event.pubkey.to_hex() == author.viewer
        } else {
            author.owns(event)
                || authorization.is_some_and(|auth| {
                    auth.owner_id == author.viewer
                        && author.displayed_author(event) == auth.agent_id
                })
        };
        if !allowed {
            return Err(Invalid(
                "Only your own messages and reactions can be removed",
            ));
        }
        targets.push(event);
    }
    let channel = |event: &Event| -> Option<String> {
        first_tag(event, "h").map(str::to_owned).or_else(|| {
            (event.kind.as_u16() == kinds::REACTION)
                .then(|| find(first_tag(event, "e").unwrap_or("")))
                .flatten()
                .and_then(|target| first_tag(target, "h"))
                .map(str::to_owned)
        })
    };
    let channel_id = channel(targets[0]);
    let Some(channel_id) = channel_id.filter(|channel_id| {
        targets
            .iter()
            .all(|event| channel(event).as_deref() == Some(channel_id.as_str()))
    }) else {
        return Err(Invalid("Removal must belong to one loaded conversation"));
    };
    Ok(targets
        .into_iter()
        .map(|event| Draft {
            kind: kinds::DELETION,
            content: String::new(),
            tags: vec![
                tag(&["h", &channel_id]),
                tag(&["e", &event.id.to_hex()]),
                tag(&["k", &event.kind.as_u16().to_string()]),
            ],
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Report (§5d.10)

/// NIP-56 types the relay accepts for message reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportType {
    Spam,
    Profanity,
    Nudity,
    Impersonation,
    Malware,
    Illegal,
    Other,
}

impl ReportType {
    pub const ALL: [Self; 7] = [
        Self::Spam,
        Self::Profanity,
        Self::Nudity,
        Self::Impersonation,
        Self::Malware,
        Self::Illegal,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spam => "spam",
            Self::Profanity => "profanity",
            Self::Nudity => "nudity",
            Self::Impersonation => "impersonation",
            Self::Malware => "malware",
            Self::Illegal => "illegal",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// A kind-1984 report of a message. Published directly with a 10 s
/// deadline, never through the outbox: reports are not echoed.
pub fn report(
    original: &Event,
    kind: ReportType,
    note: &str,
    now: u64,
) -> Result<EventTemplate, MessageError> {
    if !matches!(
        original.kind.as_u16(),
        kinds::CHAT | kinds::AGENT_MESSAGE | kinds::DIFF
    ) {
        return Err(Invalid("Load the message before reporting it"));
    }
    Ok(EventTemplate {
        kind: kinds::REPORT,
        created_at: now,
        tags: vec![
            tag(&["p", &original.pubkey.to_hex()]),
            tag(&["e", &original.id.to_hex(), kind.as_str()]),
        ],
        content: note.trim().to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Open DM (§5e.1)

/// One to eight distinct other people (`direct-message-policy.ts`).
pub fn valid_dm_participants(pubkeys: &[String], viewer: &str) -> bool {
    let distinct: HashSet<&str> = pubkeys.iter().map(String::as_str).collect();
    (1..=8).contains(&pubkeys.len())
        && distinct.len() == pubkeys.len()
        && pubkeys.iter().all(|key| is_hex64(key) && key != viewer)
}

/// The 41010 open-DM command. The random `client` tag gives every request
/// a new ID: a duplicate is answered without a `channel_id`.
pub fn open_dm(viewer: &str, pubkeys: &[String], now: u64) -> Result<EventTemplate, MessageError> {
    if !valid_dm_participants(pubkeys, viewer) {
        return Err(Invalid("Choose between one and eight other people."));
    }
    let mut tags: Vec<Tag> = pubkeys.iter().map(|key| tag(&["p", key])).collect();
    tags.push(tag(&["client", &uuid::Uuid::new_v4().to_string()]));
    Ok(EventTemplate {
        kind: kinds::OPEN_DM,
        created_at: now,
        tags,
        content: String::new(),
    })
}

const DM_NOT_OPENED: MessageError = Invalid("The direct message could not be opened. Try again.");
const DM_INVALID: MessageError = Invalid("The relay returned an invalid direct message.");

fn lowercase_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// The DM channel ID from a 200 receipt for `event_id`:
/// `{"event_id", "accepted": true, "message": "response:{\"channel_id\":…}"}`.
pub fn parse_open_dm_receipt(body: &[u8], event_id: &EventId) -> Result<String, MessageError> {
    if body.len() > crate::outbox::MAX_RECEIPT_BYTES {
        return Err(DM_NOT_OPENED);
    }
    let receipt: serde_json::Value = serde_json::from_slice(body).map_err(|_| DM_NOT_OPENED)?;
    if receipt.get("event_id").and_then(|v| v.as_str()) != Some(event_id.to_hex().as_str())
        || receipt.get("accepted") != Some(&serde_json::Value::Bool(true))
    {
        return Err(DM_NOT_OPENED);
    }
    let payload = receipt
        .get("message")
        .and_then(|v| v.as_str())
        .and_then(|message| message.strip_prefix("response:"))
        .ok_or(DM_NOT_OPENED)?;
    let result: serde_json::Value = serde_json::from_str(payload).map_err(|_| DM_INVALID)?;
    result
        .get("channel_id")
        .and_then(|v| v.as_str())
        .filter(|id| lowercase_uuid(id))
        .map(str::to_owned)
        .ok_or(DM_INVALID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag as NostrTag, Timestamp};

    const CHANNEL: &str = "3f0c2a8e-6b1d-4c1e-9a55-0d4f6c2b9e11";

    #[test]
    fn key_limits_count_unique_keys() {
        let repeated = vec!["a".repeat(64); 40];
        assert_eq!(reference_tags(&repeated).unwrap().len(), 1);
        let many: Vec<String> = (0..33).map(|n| format!("{n:064x}")).collect();
        assert!(reference_tags(&many).is_err());
    }

    fn origin() -> RelayOrigin {
        RelayOrigin::parse("https://relay.example").unwrap()
    }

    fn signed(keys: &Keys, kind: u16, content: &str, tags: Vec<Vec<&str>>) -> Event {
        EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags.into_iter().map(|t| NostrTag::parse(t).unwrap()))
            .custom_created_at(Timestamp::from_secs(1_700_000_000))
            .finalize(keys)
            .unwrap()
    }

    fn compose<'a>(
        content: &'a str,
        mentions: &'a Mentions,
        origin: &'a RelayOrigin,
    ) -> Compose<'a> {
        Compose {
            channel_id: CHANNEL,
            content,
            mentions,
            attachments: &[],
            emoji: EmojiCatalog::Ready(&[]),
            origin,
        }
    }

    fn key(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn sha() -> String {
        "ab".repeat(32)
    }

    fn photo() -> Attachment {
        Attachment {
            name: "screenshot.png".into(),
            url: format!("https://relay.example/media/{}.png", sha()),
            mime: "image/png".into(),
            size: 48_213,
            sha256: sha(),
            voice_duration: None,
        }
    }

    #[test]
    fn message_tags_follow_the_fixed_order() {
        let origin = origin();
        let catalog = [CustomEmoji {
            shortcode: "partyparrot".into(),
            url: "https://relay.example/media/77.png".into(),
        }];
        let mentions = Mentions {
            recipients: vec![key('a'), key('a')],
            references: vec![key('c')],
            ..Default::default()
        };
        let attachments = [photo()];
        let draft = message(&Compose {
            attachments: &attachments,
            emoji: EmojiCatalog::Ready(&catalog),
            ..compose(
                "  @Alice can you check this? :partyparrot:  ",
                &mentions,
                &origin,
            )
        })
        .unwrap();
        assert_eq!(draft.kind, 9);
        assert_eq!(
            draft.content,
            format!(
                "@Alice can you check this? :partyparrot:\n\n![screenshot.png](<https://relay.example/media/{}.png>)",
                sha()
            )
        );
        assert_eq!(
            draft.tags,
            vec![
                tag(&["h", CHANNEL]),
                tag(&["p", &key('a')]),
                tag(&["mention", &key('c')]),
                tag(&["emoji", "partyparrot", "https://relay.example/media/77.png"]),
                vec![
                    "imeta".into(),
                    format!("url https://relay.example/media/{}.png", sha()),
                    "m image/png".into(),
                    "size 48213".into(),
                    format!("x {}", sha()),
                    "filename screenshot.png".into(),
                ],
            ]
        );
    }

    #[test]
    fn asks_a_mentioned_agent_for_a_model_and_effort_and_only_a_mentioned_one() {
        let origin = origin();
        let picked = |pubkey: String, model: Option<&str>, effort: Option<&str>| Mentions {
            recipients: vec![key('a')],
            references: Vec::new(),
            picks: [(
                pubkey,
                RunPick {
                    model: model.map(str::to_owned),
                    effort: effort.map(str::to_owned),
                },
            )]
            .into(),
        };
        let asked = picked(key('a'), Some("claude-fable-5-1[1m]"), Some("xhigh"));
        for draft in [
            message(&compose("@Honey", &asked, &origin)).unwrap(),
            reply(&compose("@Honey", &asked, &origin), &key('f'), &key('f')).unwrap(),
        ] {
            assert!(
                draft
                    .tags
                    .contains(&tag(&["model", &key('a'), "claude-fable-5-1[1m]"]))
            );
            assert!(draft.tags.contains(&tag(&["effort", &key('a'), "xhigh"])));
        }
        for refused in [
            picked(key('b'), None, Some("high")),
            picked(key('a'), None, Some("High Effort")),
            picked(key('a'), Some("two words"), None),
        ] {
            assert_eq!(
                message(&compose("@Honey", &refused, &origin)),
                Err(Invalid(
                    "A model or effort applies only to a mentioned agent"
                ))
            );
        }
    }

    #[test]
    fn empty_and_invalid_messages_are_refused() {
        let origin = origin();
        let none = Mentions::default();
        assert_eq!(
            message(&compose("  \n", &none, &origin)),
            Err(MessageError::Empty)
        );
        let too_many = Mentions {
            recipients: (0..33).map(|i| format!("{i:064x}")).collect(),
            references: Vec::new(),
            ..Default::default()
        };
        assert!(message(&compose("hi", &too_many, &origin)).is_err());
        let upper = Mentions {
            recipients: vec!["A".repeat(64)],
            references: Vec::new(),
            ..Default::default()
        };
        assert!(message(&compose("hi", &upper, &origin)).is_err());
        assert!(
            message(&Compose {
                channel_id: "",
                ..compose("hi", &none, &origin)
            })
            .is_err()
        );
        // Attachments alone are a message.
        let attachments = [photo()];
        let draft = message(&Compose {
            attachments: &attachments,
            ..compose("", &none, &origin)
        })
        .unwrap();
        assert!(draft.content.starts_with("![screenshot.png]"));
    }

    #[test]
    fn replies_carry_nip10_markers() {
        let origin = origin();
        let none = Mentions::default();
        let root = key('1');
        let parent = key('2');
        let direct = reply(&compose("agreed", &none, &origin), &root, &root).unwrap();
        assert_eq!(
            direct.tags,
            vec![tag(&["h", CHANNEL]), tag(&["e", &root, "", "reply"])]
        );
        let nested = reply(&compose("agreed", &none, &origin), &root, &parent).unwrap();
        assert_eq!(
            nested.tags,
            vec![
                tag(&["h", CHANNEL]),
                tag(&["e", &root, "", "root"]),
                tag(&["e", &parent, "", "reply"]),
            ]
        );
        assert!(reply(&compose("x", &none, &origin), "nope", &root).is_err());
        assert!(reply(&compose("x", &none, &origin), &root, "nope").is_err());
    }

    #[test]
    fn attachments_are_validated_and_labelled() {
        let origin = origin();
        let mut doc = photo();
        doc.name = "a/b\u{202e}*report*.pdf".into();
        doc.mime = "application/PDF".into();
        doc.url = format!("/media/{}", sha());
        let (body, tags) = attachment_message("", &[doc.clone()], &origin).unwrap();
        assert_eq!(
            body,
            format!(
                "[a b \\*report\\*.pdf](<https://relay.example/media/{}>)",
                sha()
            )
        );
        assert_eq!(tags[0][2], "m application/pdf");
        assert_eq!(tags[0][5], "filename a b *report*.pdf");

        let long = Attachment {
            name: "é".repeat(200),
            ..photo()
        };
        let (_, tags) = attachment_message("", &[long], &origin).unwrap();
        let name = tags[0][5].strip_prefix("filename ").unwrap();
        assert_eq!(name.len(), 254);

        let blank = Attachment {
            name: " \u{0007} ".into(),
            ..photo()
        };
        let (_, tags) = attachment_message("", &[blank], &origin).unwrap();
        assert_eq!(tags[0][5], "filename File");

        for bad in [
            Attachment {
                url: format!("https://elsewhere.example/media/{}.png", sha()),
                ..photo()
            },
            Attachment {
                url: format!("https://relay.example/media/{}.png?x=1", sha()),
                ..photo()
            },
            Attachment {
                url: format!("https://relay.example/files/{}.png", sha()),
                ..photo()
            },
            Attachment {
                sha256: "AB".repeat(32),
                ..photo()
            },
            Attachment {
                mime: "image/gif".into(),
                size: 11 * MIB,
                ..photo()
            },
            Attachment { size: 0, ..photo() },
            Attachment {
                voice_duration: Some(301.0),
                ..photo()
            },
        ] {
            assert!(attachment_message("x", &[bad], &origin).is_err());
        }
    }

    #[test]
    fn voice_notes_write_duration_but_never_waveform() {
        let origin = origin();
        let voice = Attachment {
            name: "voice-note-1.mp4".into(),
            url: format!("https://relay.example/media/{}.mp4", sha()),
            mime: "video/mp4".into(),
            voice_duration: Some(12.5),
            ..photo()
        };
        let (_, tags) = attachment_message("", &[voice], &origin).unwrap();
        assert_eq!(tags[0].last().unwrap(), "duration 12.5");
        assert!(!tags[0].iter().any(|field| field.starts_with("waveform")));
    }

    #[test]
    fn emoji_references_skip_links_and_unknown_codes() {
        assert_eq!(
            referenced_emoji(
                ":PARTY: :party: https://a.test/:link: http://localhost:3000/:dev: :other:"
            ),
            vec!["party", "other"]
        );
        let catalog = [CustomEmoji {
            shortcode: "party".into(),
            url: "https://relay.example/media/p.png".into(),
        }];
        assert_eq!(
            emoji_tags(":unknown:party:", EmojiCatalog::Ready(&catalog)).unwrap(),
            vec![tag(&[
                "emoji",
                "party",
                "https://relay.example/media/p.png"
            ])]
        );
        // Overlapping tokens: the first match consumes the shared colon.
        assert_eq!(
            emoji_tags(":party:party:", EmojiCatalog::Ready(&catalog))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            emoji_tags("plain", EmojiCatalog::Loading).unwrap(),
            Vec::<Tag>::new()
        );
        assert_eq!(
            emoji_tags(":party:", EmojiCatalog::Loading),
            Err(MessageError::EmojiLoading)
        );
        assert!(matches!(
            emoji_tags(
                ":party:",
                EmojiCatalog::Unavailable("catalog may be incomplete")
            ),
            Err(MessageError::EmojiUnavailable(_))
        ));
        assert_eq!(
            emoji_tags(&format!(":{}:", "x".repeat(65)), EmojiCatalog::Loading).unwrap(),
            Vec::<Tag>::new()
        );
        assert_eq!(normalize_shortcode(" :PaRtY: ").as_deref(), Some("party"));
        assert_eq!(normalize_shortcode("bad code"), None);
    }

    #[test]
    fn reactions_are_bounded_and_tag_custom_emoji() {
        let keys = Keys::generate();
        let root = signed(&keys, 9, "React here", vec![vec!["h", CHANNEL]]);
        let party = CustomEmoji {
            shortcode: "party".into(),
            url: "nostr://party".into(),
        };
        let draft = react(&root, ":Party:", Some(&party), EmojiCatalog::Loading).unwrap();
        assert_eq!(draft.kind, 7);
        assert_eq!(
            draft.tags,
            vec![
                tag(&["h", CHANNEL]),
                tag(&["e", &root.id.to_hex()]),
                tag(&["emoji", "party", "nostr://party"]),
            ]
        );
        let plain = react(&root, " 👍 ", None, EmojiCatalog::Loading).unwrap();
        assert_eq!(plain.content, "👍");
        assert_eq!(plain.tags.len(), 2);
        assert!(valid_reaction_content(&"x".repeat(64)));
        assert!(!valid_reaction_content(&"x".repeat(65)));
        assert!(valid_reaction_content(&format!(":{}:", "x".repeat(64))));
        assert_eq!(
            react(&root, &"x".repeat(65), None, EmojiCatalog::Loading),
            Err(Invalid("Reaction is too long"))
        );
        assert_eq!(
            react(&root, " ", None, EmojiCatalog::Loading),
            Err(MessageError::Empty)
        );
        let profile = signed(&keys, 0, "{}", vec![]);
        assert!(react(&profile, "👍", None, EmojiCatalog::Loading).is_err());
    }

    #[test]
    fn deletions_emit_one_event_per_target() {
        let viewer = Keys::generate();
        let other = Keys::generate();
        let author = Author {
            viewer: &viewer.public_key().to_hex(),
            signing_authority: None,
        };
        let mine = signed(&viewer, 9, "mine", vec![vec!["h", CHANNEL]]);
        let target = mine.id.to_hex();
        // A reaction without its own `h` takes the channel of its target.
        let reaction = signed(&viewer, 7, "👍", vec![vec!["e", &target]]);
        let theirs = signed(&other, 9, "theirs", vec![vec!["h", CHANNEL]]);
        let elsewhere = signed(&viewer, 9, "elsewhere", vec![vec!["h", "other"]]);
        let events = [&mine, &reaction, &theirs, &elsewhere];
        let find = |id: &str| events.iter().copied().find(|e| e.id.to_hex() == id);

        let drafts = remove(
            &author,
            &[target.clone(), reaction.id.to_hex(), target.clone()],
            find,
            None,
        )
        .unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(
            drafts[0].tags,
            vec![tag(&["h", CHANNEL]), tag(&["e", &target]), tag(&["k", "9"])]
        );
        assert_eq!(drafts[1].tags[2], tag(&["k", "7"]));
        assert!(drafts.iter().all(|d| d.kind == 5 && d.content.is_empty()));

        assert!(remove(&author, &[theirs.id.to_hex()], find, None).is_err());
        assert!(
            remove(
                &author,
                &[target.clone(), elsewhere.id.to_hex()],
                find,
                None
            )
            .is_err()
        );
        assert!(remove(&author, &[], find, None).is_err());
        assert!(remove(&author, &[key('f')], find, None).is_err());

        // An agent's verified owner may remove its messages.
        let agent = other.public_key().to_hex();
        let owner = AgentOwner {
            agent_id: &agent,
            owner_id: author.viewer,
        };
        assert_eq!(
            remove(&author, &[theirs.id.to_hex()], find, Some(owner))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn relay_attributed_authors_own_their_messages() {
        let relay = Keys::generate();
        let viewer = Keys::generate();
        let viewer_hex = viewer.public_key().to_hex();
        let relay_hex = relay.public_key().to_hex();
        let attributed = signed(
            &relay,
            9,
            "workflow",
            vec![vec!["p", &viewer_hex], vec!["h", CHANNEL]],
        );
        let relay_key = relay.public_key();
        assert_eq!(
            message_author(&attributed, Some(&relay_key)).to_hex(),
            viewer_hex
        );
        assert_eq!(message_author(&attributed, None).to_hex(), relay_hex);
        let actor = signed(
            &relay,
            9,
            "x",
            vec![
                vec!["p", &key('a')],
                vec!["actor", &viewer_hex],
                vec!["h", CHANNEL],
            ],
        );
        assert_eq!(
            message_author(&actor, Some(&relay_key)).to_hex(),
            viewer_hex
        );
        let author = Author {
            viewer: &viewer_hex,
            signing_authority: Some(&relay_hex),
        };
        assert!(
            edit(
                &author,
                &attributed,
                &attributed,
                "fixed",
                EmojiCatalog::Loading
            )
            .is_ok()
        );
    }

    #[test]
    fn edits_copy_current_media_and_keep_recipients() {
        let viewer = Keys::generate();
        let author = Author {
            viewer: &viewer.public_key().to_hex(),
            signing_authority: None,
        };
        let imeta = format!("url https://relay.example/media/{}.png", sha());
        let original = signed(
            &viewer,
            9,
            "typo",
            vec![
                vec!["h", CHANNEL],
                vec!["p", &key('a')],
                vec!["imeta", &imeta],
            ],
        );
        let id = original.id.to_hex();
        let catalog = [CustomEmoji {
            shortcode: "partyparrot".into(),
            url: "https://e/p.png".into(),
        }];
        let draft = edit(
            &author,
            &original,
            &original,
            " fixed :partyparrot: ",
            EmojiCatalog::Ready(&catalog),
        )
        .unwrap();
        assert_eq!(draft.kind, 40003);
        assert_eq!(draft.content, "fixed :partyparrot:");
        assert_eq!(
            draft.tags,
            vec![
                tag(&["h", CHANNEL]),
                tag(&["e", &id]),
                tag(&["imeta", &imeta]),
                tag(&["emoji", "partyparrot", "https://e/p.png"]),
            ]
        );
        // A later own edit that carried media is a valid source.
        let earlier = signed(
            &viewer,
            40003,
            "first",
            vec![
                vec!["h", CHANNEL],
                vec!["e", &id],
                vec!["imeta", "url other"],
            ],
        );
        let draft = edit(
            &author,
            &original,
            &earlier,
            "second",
            EmojiCatalog::Loading,
        )
        .unwrap();
        assert_eq!(draft.tags[2], tag(&["imeta", "url other"]));
        // Edits without media cannot stand in for the original's attachments.
        let bare = signed(
            &viewer,
            40003,
            "x",
            vec![vec!["h", CHANNEL], vec!["e", &id]],
        );
        assert!(edit(&author, &original, &bare, "y", EmojiCatalog::Loading).is_err());
        let someone = Keys::generate();
        let theirs = signed(&someone, 9, "theirs", vec![vec!["h", CHANNEL]]);
        assert!(edit(&author, &theirs, &theirs, "mine now", EmojiCatalog::Loading).is_err());
    }

    #[test]
    fn sent_from_thread_copies_without_renotifying() {
        let viewer = Keys::generate();
        let author = Author {
            viewer: &viewer.public_key().to_hex(),
            signing_authority: None,
        };
        let root = key('1');
        let reply_event = signed(
            &viewer,
            9,
            "Hello @Other",
            vec![
                vec!["h", CHANNEL],
                vec!["e", &root, "", "reply"],
                vec!["p", &key('a')],
                vec!["imeta", "url https://x/media/a.png"],
                vec!["link-preview", "https://x"],
            ],
        );
        let mentions = [key('a'), key('b')];
        let emoji = [CustomEmoji {
            shortcode: "party".into(),
            url: "https://e/p.png".into(),
        }];
        let root_content = format!(
            "**Updated** [layout](https://example.com) ||secret||\n\n{}",
            "😀".repeat(80)
        );
        let row = SentFromThread {
            reply: &reply_event,
            attachment_source: &reply_event,
            content: "Hello @Other",
            attachment_urls: &[],
            mentions: &mentions,
            edited: false,
            emoji: &emoji,
            delivery: Some(Delivery::Accepted),
            root: Some(ThreadRoot {
                id: &root,
                channel_id: CHANNEL,
                content: &root_content,
            }),
        };
        let draft = sent_from_thread(&author, &row).unwrap();
        assert_eq!(draft.kind, 9);
        let provenance = &draft.tags[1];
        assert_eq!(provenance[0], "buzz:sent-from-thread");
        assert_eq!(provenance[1], root);
        let excerpt = &provenance[2];
        assert!(excerpt.starts_with("Updated layout 😀"));
        assert!(!excerpt.contains("secret") && !excerpt.contains("https:"));
        assert_eq!(excerpt.chars().count(), 64);
        assert!(excerpt.ends_with('…'));
        assert_eq!(
            draft.tags[2..],
            [
                tag(&["mention", &key('a')]),
                tag(&["mention", &key('b')]),
                tag(&["emoji", "party", "https://e/p.png"]),
                tag(&["imeta", "url https://x/media/a.png"]),
                tag(&["link-preview", "https://x"]),
            ]
        );
        assert!(!draft.tags.iter().any(|t| t[0] == "p" || t[0] == "e"));

        // A root from another conversation gives no excerpt.
        let other = SentFromThread {
            root: Some(ThreadRoot {
                id: &root,
                channel_id: "elsewhere",
                content: "Unrelated",
            }),
            ..row
        };
        assert_eq!(
            sent_from_thread(&author, &other).unwrap().tags[1],
            tag(&["buzz:sent-from-thread", &root])
        );
        // Edited replies drop the mention bindings.
        let edited = SentFromThread {
            edited: true,
            ..row
        };
        assert!(
            !sent_from_thread(&author, &edited)
                .unwrap()
                .tags
                .iter()
                .any(|t| t[0] == "mention")
        );
        // Attachment-only replies use the URLs as the body.
        let urls = ["https://x/media/a.png".to_owned()];
        let empty = SentFromThread {
            content: "  ",
            attachment_urls: &urls,
            ..row
        };
        assert_eq!(sent_from_thread(&author, &empty).unwrap().content, urls[0]);
        // Pending rows and other people's replies are refused.
        let pending = SentFromThread {
            delivery: Some(Delivery::Unknown),
            ..row
        };
        assert!(sent_from_thread(&author, &pending).is_err());
        let someone = Keys::generate();
        let foreign = signed(
            &someone,
            9,
            "x",
            vec![vec!["h", CHANNEL], vec!["e", &root, "", "reply"]],
        );
        let theirs = SentFromThread {
            reply: &foreign,
            attachment_source: &foreign,
            ..row
        };
        assert!(sent_from_thread(&author, &theirs).is_err());
        // A top-level message is not a thread reply.
        let top = signed(&viewer, 9, "x", vec![vec!["h", CHANNEL]]);
        let not_reply = SentFromThread {
            reply: &top,
            attachment_source: &top,
            ..row
        };
        assert!(sent_from_thread(&author, &not_reply).is_err());
    }

    #[test]
    fn reports_name_the_signer_and_reason() {
        let keys = Keys::generate();
        let message = signed(&keys, 9, "spam!", vec![vec!["h", CHANNEL]]);
        let report = report(&message, ReportType::Spam, "  buy now  ", 1_700_000_100).unwrap();
        assert_eq!(report.kind, 1984);
        assert_eq!(report.created_at, 1_700_000_100);
        assert_eq!(report.content, "buy now");
        assert_eq!(
            report.tags,
            vec![
                tag(&["p", &keys.public_key().to_hex()]),
                tag(&["e", &message.id.to_hex(), "spam"]),
            ]
        );
        assert_eq!(ReportType::parse("malware"), Some(ReportType::Malware));
        assert_eq!(ReportType::parse("rude"), None);
        let profile = signed(&keys, 0, "{}", vec![]);
        assert!(super::report(&profile, ReportType::Other, "", 1).is_err());
    }

    #[test]
    fn open_dm_shapes_and_receipts() {
        let viewer = key('f');
        let others = [key('a'), key('b')];
        let template = open_dm(&viewer, &others, 1_700_000_000).unwrap();
        assert_eq!(template.kind, 41010);
        assert_eq!(template.content, "");
        assert_eq!(template.tags[0], tag(&["p", &others[0]]));
        assert_eq!(template.tags[1], tag(&["p", &others[1]]));
        assert_eq!(template.tags[2][0], "client");
        assert!(uuid::Uuid::parse_str(&template.tags[2][1]).is_ok());
        let again = open_dm(&viewer, &others, 1_700_000_000).unwrap();
        assert_ne!(template.tags[2][1], again.tags[2][1]);

        for bad in [
            vec![],
            vec![key('a'), key('a')],
            vec![viewer.clone()],
            vec!["A".repeat(64)],
            (0..9).map(|i| format!("{i:064x}")).collect(),
        ] {
            assert!(open_dm(&viewer, &bad, 1).is_err());
        }

        let id = EventId::from_hex(&key('e')).unwrap();
        let channel = "0f0c2a8e-6b1d-4c1e-9a55-0d4f6c2b9e11";
        let body = |event_id: &str, accepted: bool, message: &str| {
            serde_json::to_vec(&serde_json::json!({
                "event_id": event_id, "accepted": accepted, "message": message,
            }))
            .unwrap()
        };
        let good = format!(r#"response:{{"channel_id":"{channel}","created":true}}"#);
        assert_eq!(
            parse_open_dm_receipt(&body(&key('e'), true, &good), &id).unwrap(),
            channel
        );
        assert_eq!(
            parse_open_dm_receipt(&body(&key('d'), true, &good), &id),
            Err(DM_NOT_OPENED)
        );
        assert_eq!(
            parse_open_dm_receipt(&body(&key('e'), false, &good), &id),
            Err(DM_NOT_OPENED)
        );
        assert_eq!(
            parse_open_dm_receipt(&body(&key('e'), true, "duplicate: already processed"), &id),
            Err(DM_NOT_OPENED)
        );
        let upper = format!(r#"response:{{"channel_id":"{}"}}"#, channel.to_uppercase());
        assert_eq!(
            parse_open_dm_receipt(&body(&key('e'), true, &upper), &id),
            Err(DM_INVALID)
        );
        assert_eq!(
            parse_open_dm_receipt(&body(&key('e'), true, "response:nope"), &id),
            Err(DM_INVALID)
        );
    }
}
