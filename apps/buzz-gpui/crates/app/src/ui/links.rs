//! Links in message bodies (`MessageLink.tsx`, `bundled/links/InlineLink.tsx`,
//! `channel-reference.ts`): what kind of place a link points to, its icon
//! and label, the chip with its tooltip, context menu and hover card, and
//! the lookup that names channels the reader has not joined.

use super::markdown::Directory;
use super::{link_preview, navigation, notice};
use buzz_relay_client::{ChannelSummary, ChannelType, Session, parse_buzz_link};
use buzz_ui::{ActiveTheme, Icon, IconName, Menu, MenuItem, MenuSize, Tooltip, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `linkKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Web,
    Github,
    Drive,
    Figma,
    Notion,
    Slack,
    Dropbox,
    OneDrive,
    Gitlab,
    Youtube,
    Loom,
    Zoom,
    Teams,
    Document,
    Spreadsheet,
    Presentation,
    Channel,
    Message,
    Thread,
}

const SERVICES: &[(LinkKind, &[&str])] = &[
    (LinkKind::Github, &["github.com"]),
    (LinkKind::Figma, &["figma.com"]),
    (LinkKind::Notion, &["notion.so", "notion.site"]),
    (LinkKind::Slack, &["slack.com"]),
    (
        LinkKind::Dropbox,
        &["dropbox.com", "dropboxusercontent.com", "db.tt"],
    ),
    (LinkKind::OneDrive, &["onedrive.live.com", "1drv.ms"]),
    (LinkKind::Gitlab, &["gitlab.com"]),
    (LinkKind::Youtube, &["youtube.com", "youtu.be"]),
    (LinkKind::Loom, &["loom.com"]),
    (LinkKind::Zoom, &["zoom.us", "zoom.com"]),
    (
        LinkKind::Teams,
        &[
            "teams.microsoft.com",
            "teams.live.com",
            "teams.cloud.microsoft",
        ],
    ),
];

/// What a link points to, or `None` for links no renderer takes (they
/// show as plain underlined text).
pub fn link_kind(href: &str) -> Option<LinkKind> {
    let url = url::Url::parse(href).ok()?;
    match url.scheme() {
        "buzz" => {
            let link = parse_buzz_link(href)?;
            Some(if link.thread.is_some() {
                LinkKind::Thread
            } else if link.message.is_some() {
                LinkKind::Message
            } else {
                LinkKind::Channel
            })
        }
        "http" | "https" => {
            let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
            if let Some(tenant) = host.strip_suffix("-my.sharepoint.com")
                && !tenant.is_empty()
                && tenant
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Some(LinkKind::OneDrive);
            }
            if host == "drive.google.com" {
                return Some(LinkKind::Drive);
            }
            if host == "docs.google.com" {
                return Some(match url.path().split('/').nth(1) {
                    Some("document") => LinkKind::Document,
                    Some("spreadsheets") => LinkKind::Spreadsheet,
                    Some("presentation") => LinkKind::Presentation,
                    _ => LinkKind::Drive,
                });
            }
            for (kind, hosts) in SERVICES {
                if hosts
                    .iter()
                    .any(|h| host == *h || host.ends_with(&format!(".{h}")))
                {
                    return Some(*kind);
                }
            }
            Some(LinkKind::Web)
        }
        _ => None,
    }
}

pub fn icon(kind: LinkKind, private_channel: bool) -> IconName {
    match kind {
        LinkKind::Web => IconName::Globe,
        LinkKind::Github => IconName::GithubLogo,
        LinkKind::Drive => IconName::GoogleDriveLogo,
        LinkKind::Figma => IconName::FigmaLogo,
        LinkKind::Notion => IconName::NotionLogo,
        LinkKind::Slack => IconName::SlackLogo,
        LinkKind::Dropbox => IconName::DropboxLogo,
        LinkKind::OneDrive => IconName::OneDriveLogo,
        LinkKind::Gitlab => IconName::GitlabLogo,
        LinkKind::Youtube => IconName::YoutubeLogo,
        LinkKind::Loom => IconName::VideoCamera,
        LinkKind::Zoom => IconName::VideoConference,
        LinkKind::Teams => IconName::MicrosoftTeamsLogo,
        LinkKind::Document => IconName::FileText,
        LinkKind::Spreadsheet => IconName::Table,
        LinkKind::Presentation => IconName::Presentation,
        LinkKind::Message => IconName::ChatCircle,
        LinkKind::Channel if private_channel => IconName::Lock,
        LinkKind::Channel => IconName::Hash,
        LinkKind::Thread => IconName::ChatsCircle,
    }
}

/// How a chip reads: the part kept with the icon, and the rest, which
/// wraps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub lead: String,
    pub rest: String,
    /// The authored content stands as is (no lead split, no truncation).
    pub content: bool,
}

/// `LinkLabel`: `label` is the authored label (`None` for a raw
/// destination), `contextual` the community's name for the place, and
/// `content` the authored text shown when it is not a raw destination.
pub fn label(href: &str, kind: LinkKind, label: Option<&str>, contextual: Option<&str>) -> Label {
    let mut text = label.unwrap_or(href).to_string();
    if text == href
        && let Some(contextual) = contextual
    {
        text = contextual.to_string();
    }
    if text == href {
        match kind {
            LinkKind::Channel => text = "Channel".into(),
            LinkKind::Message => text = "Message".into(),
            LinkKind::Thread => text = "Thread".into(),
            _ => {}
        }
    }
    if kind == LinkKind::Channel
        && let Some(stripped) = text.strip_prefix('#')
    {
        text = stripped.to_string();
    }
    let raw = raw_destination(&text, href);
    if label.is_some() && !raw {
        return Label {
            lead: String::new(),
            rest: text,
            content: true,
        };
    }
    if raw {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() > 45 {
            text = chars[..44].iter().collect::<String>() + "…";
        }
    }
    let lead_end = lead_len(&text);
    let lead: String = text[..lead_end].chars().take(24).collect();
    let rest = text[lead.len()..].to_string();
    Label {
        lead,
        rest,
        content: false,
    }
}

/// `/^(?:https?:\/\/)?[^/\s]+/`, else the whole label.
fn lead_len(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    let scheme = ["https://", "http://"]
        .iter()
        .find(|s| lower.starts_with(**s))
        .map_or(0, |s| s.len());
    let host = text[scheme..]
        .find(|c: char| c == '/' || c.is_whitespace())
        .unwrap_or(text.len() - scheme);
    if host == 0 { text.len() } else { scheme + host }
}

/// The label is the destination itself, even with Unicode Markdown encoded
/// in the destination but not in the visible text.
fn raw_destination(text: &str, href: &str) -> bool {
    if text == href {
        return true;
    }
    link_kind(text).is_some()
        && match (url::Url::parse(text), url::Url::parse(href)) {
            (Ok(a), Ok(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
}

// ---------------------------------------------------------------------------
// Channel references

/// What a channel-only link may show about a channel (`ChannelReference`).
#[derive(Clone, Debug, PartialEq)]
pub enum Reference {
    Unknown,
    /// A completed lookup found nothing it may show: private, deleted and
    /// never-valid channels look the same.
    Withheld,
    Found {
        channel: Box<ChannelSummary>,
        joined: bool,
    },
}

/// Withheld answers are rechecked after this.
const REFERENCE_TTL: Duration = Duration::from_secs(60);
/// A failed lookup waits this long before another.
const REFERENCE_RETRY: Duration = Duration::from_secs(15);

#[derive(Default)]
struct References {
    /// `(community, channel)` → answer and when it was learned.
    answers: HashMap<(String, String), (Reference, Instant)>,
    pending: std::collections::HashSet<(String, String)>,
}

impl Global for References {}

/// `describe` + `refer`: listed channels answer at once; others are
/// looked up once and cached, then rechecked when the answer expires.
pub fn reference(directory: &Directory, id: &str, cx: &mut App) -> Reference {
    if let Some(channel) = directory.channels.iter().find(|c| c.id == id) {
        let joined = channel.members.iter().any(|m| *m == directory.viewer());
        return Reference::Found {
            channel: Box::new(channel.clone()),
            joined,
        };
    }
    let Some(session) = directory.session.clone() else {
        return Reference::Unknown;
    };
    let key = (session.origin().to_string(), id.to_string());
    let references = cx.default_global::<References>();
    let answer = references.answers.get(&key).cloned();
    let due = match &answer {
        None => true,
        Some((Reference::Withheld, at)) => at.elapsed() >= REFERENCE_TTL,
        Some((Reference::Unknown, at)) => at.elapsed() >= REFERENCE_RETRY,
        Some((Reference::Found { .. }, _)) => false,
    };
    if due && references.pending.insert(key.clone()) {
        request(session, key.clone(), cx);
    }
    answer.map_or(Reference::Unknown, |(reference, _)| reference)
}

fn request(session: Arc<Session>, key: (String, String), cx: &mut App) {
    let viewer = session.viewer().to_string();
    cx.spawn(async move |cx| {
        let result = session.resolve_channels(vec![key.1.clone()]).await;
        let answer = match result {
            Ok(found) => match found.into_iter().find(|c| c.id == key.1) {
                Some(channel) => Reference::Found {
                    joined: channel.members.contains(&viewer),
                    channel: Box::new(channel),
                },
                None => Reference::Withheld,
            },
            Err(_) => Reference::Unknown,
        };
        cx.update(|cx| {
            let references = cx.default_global::<References>();
            references.pending.remove(&key);
            references.answers.insert(key, (answer, Instant::now()));
            cx.refresh_windows();
        });
    })
    .detach();
}

/// `channelLinkLabel`: the listed channel's name for a link, with `#` for
/// channel links outside DMs.
fn directory_label(url: &str, channels: &[ChannelSummary]) -> Option<String> {
    let link = parse_buzz_link(url)?;
    let channel = channels.iter().find(|c| c.id == link.channel)?;
    let bare = channel.channel_type == Some(ChannelType::Dm) || link.message.is_some();
    Some(format!("{}{}", if bare { "" } else { "#" }, channel.name))
}

// ---------------------------------------------------------------------------
// The chip

/// StyledLabel runs of an authored label: `(range, bold, italic, strike, code)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyledLabel {
    pub text: String,
    pub runs: Vec<(Range<usize>, HighlightStyle)>,
}

/// Everything a link chip needs.
pub struct Chip<'a> {
    pub id: SharedString,
    pub url: &'a str,
    /// The authored label, `None` for a raw destination.
    pub label: Option<StyledLabel>,
    pub directory: &'a Directory,
    pub font_size: Pixels,
    pub line_height: Pixels,
    /// The widest the chip may be before its label wraps.
    pub max_width: Option<Pixels>,
    /// The message it is in, if any, which a panel it opens belongs to.
    pub origin: Option<navigation::LinkOrigin>,
}

/// How links drawn now open.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// Through panels and navigation, as a message's do.
    Normal,
    /// Web links go to the browser, never a panel, as plain Markdown
    /// anchors do (Projects bodies, agent replies).
    Browser,
    /// That, and `buzz://` links say they cannot open here (media review
    /// comments, `onOpenLink={() => false}`).
    Closed,
}

thread_local! {
    static MODE: std::cell::Cell<LinkMode> = const { std::cell::Cell::new(LinkMode::Normal) };
}

/// How links drawn now open.
pub fn mode() -> LinkMode {
    MODE.get()
}

/// Draws with links opening as `mode` says.
pub fn with_mode<T>(mode: LinkMode, draw: impl FnOnce() -> T) -> T {
    let before = MODE.replace(mode);
    let drawn = draw();
    MODE.set(before);
    drawn
}

/// Opens web link `url` as the mode it was drawn in says.
pub(crate) fn open_web(
    mode: LinkMode,
    url: &str,
    modifiers: Modifiers,
    origin: Option<navigation::LinkOrigin>,
    window: &mut Window,
    cx: &mut App,
) {
    match mode {
        LinkMode::Normal => navigation::open_web_link_from(url, modifiers, origin, window, cx),
        LinkMode::Browser | LinkMode::Closed => cx.open_url(url),
    }
}

/// Opens `buzz://` link `url` as the mode it was drawn in says: `false`
/// when it cannot open here.
fn open_buzz(mode: LinkMode, url: &str, window: &mut Window, cx: &mut App) -> bool {
    mode != LinkMode::Closed && navigation::try_open_url(url, window, cx)
}

/// A plain anchor for links no renderer takes: underlined body text.
fn plain_anchor(
    id: SharedString,
    url: &str,
    text: String,
    line: Pixels,
    origin: Option<navigation::LinkOrigin>,
    c: &buzz_ui::Colors,
) -> AnyElement {
    let target = url.to_string();
    let mode = MODE.get();
    let openable = url.starts_with("buzz:")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("mailto:");
    div()
        .id(id)
        .line_height(line)
        .text_color(c.text_standard)
        .underline()
        .when(openable, |d| {
            d.cursor_pointer().on_click(move |event, window, cx| {
                if target.starts_with("buzz:") {
                    if mode != LinkMode::Closed {
                        navigation::open_url(&target, window, cx)
                    }
                } else {
                    open_web(mode, &target, event.modifiers(), origin.clone(), window, cx)
                }
            })
        })
        .child(text)
        .into_any_element()
}

/// The link chip a body shows for `url`.
pub fn chip(chip: Chip<'_>, window: &mut Window, cx: &mut App) -> AnyElement {
    let c = *cx.colors();
    let mode = MODE.get();
    let url = chip.url;
    let authored = chip.label.as_ref().map(|l| l.text.clone());
    let kind = link_kind(url);
    // Web links must be safe to hand to the browser.
    if matches!(url::Url::parse(url), Ok(parsed) if !parsed.username().is_empty() || parsed.password().is_some())
    {
        return plain_anchor(
            chip.id,
            url,
            authored.unwrap_or_else(|| url.to_string()),
            chip.line_height,
            chip.origin.clone(),
            &c,
        );
    }
    // Links Buzz does not draw itself, such as `buzz://` entities, are
    // plain unless a renderer took them.
    let Some(kind) = kind else {
        let Some(custom) = super::renderers::link(url, cx) else {
            return plain_anchor(
                chip.id,
                url,
                authored.unwrap_or_else(|| url.to_string()),
                chip.line_height,
                chip.origin.clone(),
                &c,
            );
        };
        let target = url.to_string();
        let origin = chip.origin.clone();
        let middle = target.clone();
        let mode = MODE.get();
        return div()
            .id(chip.id)
            .cursor_pointer()
            .line_height(chip.line_height)
            .child(custom)
            .on_click(move |event, window, cx| {
                if target.starts_with("buzz:") {
                    open_buzz(mode, &target, window, cx);
                } else {
                    open_web(mode, &target, event.modifiers(), origin.clone(), window, cx)
                }
            })
            .on_mouse_down(MouseButton::Middle, move |_, window, cx| {
                if middle.starts_with("buzz:") {
                    cx.stop_propagation();
                    open_buzz(mode, &middle, window, cx);
                }
            })
            .into_any_element();
    };
    let link = parse_buzz_link(url);
    let directory = chip.directory;
    // Channel-only links name a channel the reader may not have joined.
    let referenced = link
        .as_ref()
        .filter(|l| l.message.is_none() && l.thread.is_none())
        .map(|l| l.channel.clone());
    let reference = match &referenced {
        Some(id) => reference(directory, id, cx),
        None => Reference::Unknown,
    };
    let named = match &reference {
        Reference::Found { channel, .. }
            if !channel.hidden
                && !matches!(
                    channel.channel_type,
                    Some(ChannelType::Dm | ChannelType::Session)
                ) =>
        {
            Some(channel.clone())
        }
        _ => None,
    };
    let withheld = reference == Reference::Withheld;
    let listed = link
        .as_ref()
        .and_then(|l| directory.channels.iter().find(|c| c.id == l.channel));
    let private_channel =
        listed.is_some_and(|c| c.private) || withheld || named.as_ref().is_some_and(|c| c.private);
    let directory_label = directory_label(url, &directory.channels);
    let display = authored.clone().or(directory_label).or_else(|| {
        named
            .as_ref()
            .map(|c| format!("#{}", c.name))
            .or_else(|| withheld.then(|| "Private channel".to_string()))
    });
    // Session chip: "Session · {id8}" for a session the viewer belongs to.
    let session_chip = link.as_ref().is_some_and(|l| {
        l.message.is_none()
            && authored.as_deref() == Some(&format!("Session · {}", short(&l.channel)))
            && directory.channels.iter().any(|c| {
                c.id == l.channel
                    && c.channel_type == Some(ChannelType::Session)
                    && !c.cached
                    && !c.read_only
                    && c.members.iter().any(|m| m == directory.viewer())
            })
    });
    // A contributed link renderer outranks the bundled one, except for a
    // Session chip, which is always Buzz's (`MessageLink` `anchor`). It
    // draws the chip's content; Buzz keeps the anchor, its menu and hover
    // cards.
    let custom = (!session_chip)
        .then(|| super::renderers::link(url, cx))
        .flatten();
    let preview = link
        .as_ref()
        .and_then(|l| l.message.map(|m| (l.channel.clone(), m)));
    let tooltip = if withheld {
        Some("Private channel. You aren’t a member, or it no longer exists.".to_string())
    } else if preview.is_none() && named.is_none() {
        Some(url.to_string())
    } else {
        None
    };
    // An authored label stays; a withheld raw link says "Private channel".
    let content: Option<StyledLabel> = if withheld && chip.label.is_none() {
        Some(StyledLabel {
            text: "Private channel".into(),
            runs: Vec::new(),
        })
    } else {
        chip.label.clone()
    };
    // Buzz's chips are the Links plugin's: off, a link is a plain anchor
    // with its label, `#channel` or URL.
    if custom.is_none() && !crate::plugins::enabled("buzz.links") {
        return plain_anchor(
            chip.id,
            url,
            display.clone().unwrap_or_else(|| url.to_string()),
            chip.line_height,
            chip.origin.clone(),
            &c,
        );
    }
    let label_text = label(url, kind, authored.as_deref(), display.as_deref());
    let icon_size = f32::from(chip.font_size);
    let icon = Icon::new(icon(kind, kind == LinkKind::Channel && private_channel))
        .size(icon_size)
        .color(c.text_link);
    let id = chip.id.clone();
    let external = !matches!(
        kind,
        LinkKind::Channel | LinkKind::Message | LinkKind::Thread
    );
    let gap = if kind == LinkKind::Channel { 0. } else { 3. };
    let body: AnyElement = if let Some(custom) = custom {
        custom
    } else if session_chip {
        let channel = link.as_ref().map(|l| short(&l.channel)).unwrap_or_default();
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .whitespace_nowrap()
            .text_body_sm()
            .text_color(c.text_standard)
            .child(Icon::new(IconName::FileText).size(icon_size))
            .child(div().text_color(c.text_subtle).child("Session"))
            .child(format!("· {channel}"))
            .into_any_element()
    } else if let Some(content) = content.filter(|_| label_text.content || withheld) {
        // Authored content keeps its formatting; a channel's `#` is the icon.
        let text = match content
            .text
            .strip_prefix('#')
            .filter(|_| kind == LinkKind::Channel)
        {
            Some(text) => StyledText::new(text.to_string()).into_any_element(),
            None => StyledText::new(content.text.clone())
                .with_highlights(content.runs.clone())
                .into_any_element(),
        };
        div()
            .flex()
            .items_center()
            .min_w_0()
            .child(div().flex_none().mr(px(gap)).child(icon))
            .child(div().min_w_0().child(text))
            .into_any_element()
    } else {
        // Centred, not baseline-aligned: the lead's box is as tall as its
        // icon, so on baselines the host sat lower than the rest.
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .whitespace_nowrap()
                    .gap(px(gap))
                    .child(icon)
                    .child(label_text.lead.clone()),
            )
            .when(!label_text.rest.is_empty(), |d| {
                d.child(label_text.rest.clone())
            })
            .into_any_element()
    };
    let target = url.to_string();
    let origin = chip.origin.clone();
    let middle = target.clone();
    let internal = !external;
    // "This Buzz link couldn’t be opened here." after a failed open.
    let unavailable = window.use_keyed_state(
        SharedString::from(format!("{id}-unavailable")),
        cx,
        |_, _| false,
    );
    let shown_unavailable = *unavailable.read(cx);
    let (open_state, middle_state) = (unavailable.clone(), unavailable);
    let menu_state =
        window.use_keyed_state(SharedString::from(format!("{id}-menu")), cx, |_, _| {
            None::<OpenMenu>
        });
    let menu_at = *menu_state.read(cx);
    let menu_focus = window
        .use_keyed_state(
            SharedString::from(format!("{id}-menu-focus")),
            cx,
            |_, cx| cx.focus_handle(),
        )
        .read(cx)
        .clone();
    let hover_state =
        window.use_keyed_state(SharedString::from(format!("{id}-hover")), cx, |_, _| {
            HoverCard::default()
        });
    // A link is a tab stop, as an anchor is; Enter opens it, and the
    // context-menu key or Shift+F10 opens an external link's menu under it.
    let focus = window
        .use_keyed_state(SharedString::from(format!("{id}-focus")), cx, |_, cx| {
            cx.focus_handle().tab_stop(true)
        })
        .read(cx)
        .clone();
    let bounds = window.use_keyed_state(SharedString::from(format!("{id}-bounds")), cx, |_, _| {
        Bounds::<Pixels>::default()
    });
    let ring = focus.is_focused(window) && window.last_input_was_keyboard();
    let mut element = div()
        .id(id.clone())
        .track_focus(&focus)
        .relative()
        .max_w_full()
        .when_some(chip.max_width, |d, width| d.max_w(width))
        .px(px(2.))
        .mx(px(-2.))
        .rounded(px(4.))
        .line_height(chip.line_height)
        .cursor_pointer()
        .map(|d| {
            if session_chip {
                d.px(px(4.))
                    .bg(c.affordance_subtle)
                    .hover(|s| s.bg(c.affordance_subtle_hover))
            } else {
                d.text_color(c.text_link)
                    .hover(|s| s.bg(c.affordance_link_hover))
            }
        })
        .when_some(tooltip.filter(|_| menu_at.is_none()), |d, text| {
            d.tooltip(Tooltip::text(text))
        })
        .child(body)
        .child({
            let bounds = bounds.clone();
            canvas(
                move |area, _, cx| bounds.update(cx, |bounds, _| *bounds = area),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        })
        .when(ring, |d| {
            // The 2px focus outline, 3px out (`:focus-visible`).
            d.child(
                div()
                    .absolute()
                    .top(px(-5.))
                    .bottom(px(-5.))
                    .left(px(-5.))
                    .right(px(-5.))
                    .rounded(px(6.))
                    .border_2()
                    .border_color(c.border_focus),
            )
        })
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            if internal {
                let opened = open_buzz(mode, &target, window, cx);
                open_state.update(cx, |state, cx| {
                    *state = !opened;
                    cx.notify();
                });
            } else {
                open_web(mode, &target, event.modifiers(), origin.clone(), window, cx)
            }
        });
    if internal {
        element = element.on_mouse_down(MouseButton::Middle, move |_, window, cx| {
            cx.stop_propagation();
            let opened = open_buzz(mode, &middle, window, cx);
            middle_state.update(cx, |state, cx| {
                *state = !opened;
                cx.notify();
            });
        });
    } else {
        let (state, pointer_focus) = (menu_state.clone(), menu_focus.clone());
        element = element.on_mouse_down(MouseButton::Right, move |event, window, cx| {
            cx.stop_propagation();
            let at = event.position;
            state.update(cx, |open, cx| {
                *open = Some(OpenMenu {
                    at,
                    highlight: None,
                });
                cx.notify();
            });
            pointer_focus.focus(window, cx);
        });
        let (state, key_focus) = (menu_state.clone(), menu_focus.clone());
        element = element.on_key_down(move |event: &KeyDownEvent, window, cx| {
            let keys = &event.keystroke;
            if keys.key == "menu" || (keys.key == "f10" && keys.modifiers.shift) {
                cx.stop_propagation();
                let at = bounds.read(cx).bottom_left();
                state.update(cx, |open, cx| {
                    *open = Some(OpenMenu {
                        at,
                        highlight: Some(0),
                    });
                    cx.notify();
                });
                key_focus.focus(window, cx);
            }
        });
    }
    // Hover cards: message links preview the message; channel links the
    // channel's public details.
    let card: Option<AnyElement> = if let Some((channel, message)) = &preview {
        directory.session.clone().map(|session| {
            link_preview::message_card(
                SharedString::from(format!("{id}-card")),
                session,
                channel,
                *message,
                directory,
                url,
                window,
                cx,
            )
        })
    } else if let (Some(channel), Reference::Found { joined, .. }) = (&named, &reference) {
        Some(link_preview::channel_card(channel, *joined, url, cx))
    } else {
        None
    };
    let hovering = card.is_some();
    if hovering {
        let state = hover_state.clone();
        element = element.on_hover(move |hovered, window, cx| {
            HoverCard::hover(&state, *hovered, window, cx);
        });
    }
    let shown = hover_state.read(cx).open;
    let mut wrapper = div()
        .relative()
        .flex()
        .items_baseline()
        .child(element)
        .when(shown_unavailable, |d| {
            d.child(
                div()
                    .whitespace_nowrap()
                    .text_color(c.text_standard)
                    .child(" This Buzz link couldn’t be opened here."),
            )
        });
    if let (true, Some(card)) = (shown, card) {
        let state = hover_state.clone();
        wrapper = wrapper.child(
            div().absolute().left_0().top_0().child(deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .offset(point(px(0.), px(-6.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id(SharedString::from(format!("{id}-card-hover")))
                            .occlude()
                            .on_hover(move |hovered, window, cx| {
                                HoverCard::hover(&state, *hovered, window, cx)
                            })
                            .child(card),
                    ),
            )),
        );
    }
    if let Some(at) = menu_at {
        wrapper = wrapper.child(link_menu(id, url, at, menu_state, (menu_focus, focus)));
    }
    wrapper.into_any_element()
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A hover card's open state: 250ms to open, and it stays while the
/// pointer moves from the link onto the card.
#[derive(Default)]
pub struct HoverCard {
    pub open: bool,
    over: bool,
    generation: u64,
}

impl HoverCard {
    pub fn hover(state: &Entity<Self>, hovered: bool, window: &mut Window, cx: &mut App) {
        let generation = state.update(cx, |card, _| {
            card.over = hovered;
            card.generation += 1;
            card.generation
        });
        let state = state.clone();
        let delay = if hovered { 250 } else { 120 };
        window
            .spawn(cx, async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(delay))
                    .await;
                let _ = cx.update(|_, cx| {
                    state.update(cx, |card, cx| {
                        if card.generation == generation && card.open != card.over {
                            card.open = card.over;
                            cx.notify();
                        }
                    })
                });
            })
            .detach();
    }
}

/// An open link menu: where, and the keyboard's highlighted item.
#[derive(Clone, Copy)]
struct OpenMenu {
    at: Point<Pixels>,
    highlight: Option<usize>,
}

/// The external link's context menu (`MessageLink.tsx`): open in the
/// browser or copy the link. It takes focus, moves
/// with the arrow keys, and gives focus back to the link as it closes.
fn link_menu(
    id: SharedString,
    url: &str,
    open: OpenMenu,
    state: Entity<Option<OpenMenu>>,
    (menu_focus, trigger): (FocusHandle, FocusHandle),
) -> AnyElement {
    type Action = Rc<dyn Fn(&mut Window, &mut App)>;
    let mut items: Vec<(&str, &str, Option<IconName>, Action)> = Vec::new();
    let browser = url.to_string();
    items.push((
        "open",
        "Open in browser",
        Some(IconName::Browser),
        Rc::new(move |_, cx| cx.open_url(&browser)),
    ));
    let copy = url.to_string();
    items.push((
        "copy",
        "Copy link",
        Some(IconName::Copy),
        Rc::new(move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
            notice::success("Link copied", cx);
        }),
    ));
    let actions: Vec<Action> = items
        .iter()
        .map(|(_, _, _, action)| action.clone())
        .collect();
    let close = {
        let state = state.clone();
        move |refocus: bool, window: &mut Window, cx: &mut App| {
            state.update(cx, |open, cx| {
                *open = None;
                cx.notify();
            });
            if refocus {
                trigger.focus(window, cx);
            }
        }
    };
    let close = Rc::new(close);
    let menu = Menu::new()
        .size(MenuSize::Compact)
        .children(
            items
                .into_iter()
                .enumerate()
                .map(|(index, (key, label, icon, action))| {
                    let close = close.clone();
                    let item = MenuItem::new(SharedString::from(format!("{id}-{key}")), label)
                        .size(MenuSize::Compact)
                        .highlighted(open.highlight == Some(index))
                        .on_click(move |_, window, cx| {
                            close(true, window, cx);
                            action(window, cx);
                        });
                    match icon {
                        Some(icon) => item.icon(icon),
                        None => item,
                    }
                }),
        );
    let count = actions.len();
    let (outside, keys) = (close.clone(), close);
    deferred(
        anchored()
            .position_mode(AnchoredPositionMode::Window)
            .position(open.at)
            .snap_to_window_with_margin(px(8.))
            .child(
                div()
                    .track_focus(&menu_focus)
                    .occlude()
                    .on_mouse_down_out(move |_, window, cx| outside(false, window, cx))
                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                        let step = |forward: bool| {
                            Some(match (open.highlight, forward) {
                                (None, true) => 0,
                                (None, false) => count - 1,
                                (Some(i), true) => (i + 1) % count,
                                (Some(i), false) => (i + count - 1) % count,
                            })
                        };
                        let highlight = match event.keystroke.key.as_str() {
                            "escape" | "tab" => {
                                cx.stop_propagation();
                                keys(true, window, cx);
                                return;
                            }
                            "enter" | "space" => {
                                if let Some(action) = open.highlight.and_then(|i| actions.get(i)) {
                                    cx.stop_propagation();
                                    keys(true, window, cx);
                                    action(window, cx);
                                }
                                return;
                            }
                            "down" => step(true),
                            "up" => step(false),
                            "home" => Some(0),
                            "end" => Some(count - 1),
                            _ => return,
                        };
                        cx.stop_propagation();
                        state.update(cx, |menu, cx| {
                            if let Some(menu) = menu {
                                menu.highlight = highlight;
                            }
                            cx.notify();
                        });
                    })
                    .child(menu),
            ),
    )
    .with_priority(2)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn kinds_follow_hosts_not_paths() {
        assert_eq!(link_kind("https://github.com/a/b"), Some(LinkKind::Github));
        assert_eq!(link_kind("https://x.com/github.com"), Some(LinkKind::Web));
        assert_eq!(link_kind("https://1drv.ms/x"), Some(LinkKind::OneDrive));
        assert_eq!(
            link_kind("https://contoso-my.sharepoint.com/personal"),
            Some(LinkKind::OneDrive)
        );
        assert_eq!(
            link_kind("https://contoso.sharepoint.com/x"),
            Some(LinkKind::Web)
        );
        assert_eq!(
            link_kind("https://docs.google.com/forms/d/1"),
            Some(LinkKind::Drive)
        );
        assert_eq!(
            link_kind("https://docs.google.com/document/d/1"),
            Some(LinkKind::Document)
        );
        assert_eq!(
            link_kind("https://dl.dropboxusercontent.com/x"),
            Some(LinkKind::Dropbox)
        );
        assert_eq!(link_kind("https://us02.zoom.com/j/1"), Some(LinkKind::Zoom));
        assert_eq!(
            link_kind("https://teams.cloud.microsoft/l/1"),
            Some(LinkKind::Teams)
        );
        assert_eq!(link_kind("buzz://channel/general"), Some(LinkKind::Channel));
        assert_eq!(link_kind("buzz://join?relay=x"), None);
        assert_eq!(link_kind("mailto:a@b.c"), None);
    }

    #[test]
    fn raw_destinations_shorten_at_45() {
        let url45 = format!("https://example.com/{}", "a".repeat(25));
        assert_eq!(url45.chars().count(), 45);
        let shown = label(&url45, LinkKind::Web, None, None);
        assert_eq!(format!("{}{}", shown.lead, shown.rest), url45);
        let url46 = format!("{url45}b");
        let shown = label(&url46, LinkKind::Web, None, None);
        let full = format!("{}{}", shown.lead, shown.rest);
        assert_eq!(full.chars().count(), 45);
        assert!(full.ends_with('…'));
        assert_eq!(shown.lead, "https://example.com");
    }

    #[test]
    fn buzz_links_fall_back_to_generic_labels() {
        let url = "buzz://channel/abc";
        assert_eq!(label(url, LinkKind::Channel, None, None).lead, "Channel");
        assert_eq!(
            label(url, LinkKind::Channel, None, Some("#general")).lead,
            "general"
        );
        let shown = label(url, LinkKind::Channel, Some("my label"), None);
        assert!(shown.content);
        assert_eq!(shown.rest, "my label");
    }
}
