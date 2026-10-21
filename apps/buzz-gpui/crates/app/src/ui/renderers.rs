//! Renderer registries (`conversation/contracts.ts`, `MessageBody.tsx`,
//! `MessageLink.tsx`, `InlineText.tsx`, `AttachmentView.tsx`). buzz-app's
//! plugins contribute message, link, inline and attachment renderers; this
//! port compiles its bundled features in, so the registries are internal
//! extension points they register through.
//!
//! Among the renderers that match, the lowest order wins, earliest
//! registration first on ties. A renderer that declines (`None`) or
//! panics leaves the host rendering in place (`ContributionBoundary`).

use super::markdown::Directory;
use super::media;
use buzz_relay_client::Media;
use buzz_relay_client::fold::{Attachment, CustomEmoji, MessageRow};
use gpui_kit::*;
use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;

/// Replaces a message's body (diffs use one).
pub trait MessageRenderer {
    fn key(&self) -> &str;
    fn order(&self, _: &MessageRow) -> i32 {
        0
    }
    fn matches(&self, row: &MessageRow) -> bool;
    fn render(&self, row: &Arc<MessageRow>, directory: &Directory, cx: &App) -> Option<AnyElement>;
    /// It drew nothing because its answer has not come yet, so no renderer
    /// after it is asked meanwhile (buzz-app mounts only the first match).
    fn waiting(&self, _: &MessageRow) -> bool {
        false
    }
}

/// Draws a link chip's contents for the URLs it matches.
pub trait LinkRenderer {
    fn key(&self) -> &str;
    fn order(&self, url: &str) -> i32;
    fn matches(&self, url: &str) -> bool;
    fn render(&self, url: &str, cx: &App) -> Option<AnyElement>;
}

/// Draws a piece of inline text, such as a custom emoji's `:shortcode:`.
pub trait InlineRenderer {
    fn key(&self) -> &str;
    fn render(
        &self,
        text: &str,
        emoji: &[CustomEmoji],
        media: &Media,
        cx: &App,
    ) -> Option<AnyElement>;
}

/// Draws pieces of message prose it matches (buzz-app's `registerInline`
/// with `matches`), such as ticket numbers. Prose is prepared without the
/// app, so a renderer answers from what it already has: [`Self::find`]
/// returns only matches it can draw now, and [`text_changed`] asks for a
/// new preparation when that changes.
pub trait TextRenderer {
    fn key(&self) -> &str;
    /// Byte ranges of `text`, in `message`'s prose, to draw, in order and
    /// not overlapping.
    fn find(&self, text: &str, message: InMessage) -> Vec<std::ops::Range<usize>>;
    /// Draws a piece [`Self::find`] matched in the message with id
    /// `message` (hex).
    fn render(&self, matched: &str, message: &str, cx: &App) -> Option<AnyElement>;
}

/// The message prose is in (`InlineContent.message`).
#[derive(Clone, Copy)]
pub struct InMessage<'a> {
    pub row: &'a MessageRow,
    pub profiles: &'a buzz_relay_client::Profiles,
    /// The reaction drawn, when the text is one (`InlineContent.reaction`).
    pub reaction: Option<&'a buzz_relay_client::fold::ReactionGroup>,
}

/// A message action (`MessageActionContribution`): a more-menu item, and
/// optionally a marker after the byline, such as a reminder's.
pub trait MessageAction {
    fn key(&self) -> &str;
    fn title(&self) -> &str;
    fn icon(&self) -> Option<buzz_ui::IconName> {
        None
    }
    /// Whether it applies to `row`, for `viewer` (the signed-in key).
    fn matches(&self, row: &MessageRow, viewer: &str) -> bool;
    fn run(&self, row: &Arc<MessageRow>, window: &mut Window, cx: &mut App);
    fn marker(&self, _: &MessageRow, _: &App) -> Option<AnyElement> {
        None
    }
}

/// Something shown beside a message's reactions (a message-placed
/// `ComposerAccessory`), such as an agent's work on it.
pub trait MessageAccessory {
    fn key(&self) -> &str;
    fn order(&self) -> i32 {
        0
    }
    /// `open_thread`: how this place opens the message's thread
    /// (`MessageAccessoryProps.openThread`), as a timeline does and a
    /// thread does not.
    fn render(
        &self,
        row: &Arc<MessageRow>,
        open_thread: Option<&super::message::RowHandler>,
        find: Option<&super::message::FindRow>,
        cx: &App,
    ) -> Option<AnyElement>;
}

/// Replaces an attachment's player (voice notes use one).
pub trait AttachmentRenderer {
    fn key(&self) -> &str;
    fn matches(&self, attachment: &Attachment) -> bool;
    fn render(
        &self,
        attachment: &Attachment,
        id: SharedString,
        media: &Media,
        cx: &App,
    ) -> Option<AnyElement>;
    /// As [`MessageRenderer::waiting`].
    fn waiting(&self, _: &Attachment) -> bool {
        false
    }
}

#[derive(Default)]
struct Registry {
    text: Vec<Rc<dyn TextRenderer>>,
    accessories: Vec<Rc<dyn MessageAccessory>>,
    messages: Vec<Rc<dyn MessageRenderer>>,
    links: Vec<Rc<dyn LinkRenderer>>,
    inline: Vec<Rc<dyn InlineRenderer>>,
    attachments: Vec<Rc<dyn AttachmentRenderer>>,
    actions: Vec<Rc<dyn MessageAction>>,
}

/// Removes a draft attachment (`AttachmentRendererProps.onRemove`).
pub type Remove = Rc<dyn Fn(&mut App)>;

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(bundled());
    /// Drafts a renderer may remove, by attachment URL.
    static REMOVERS: RefCell<std::collections::HashMap<String, Remove>> =
        RefCell::new(std::collections::HashMap::new());
    /// Changes when prose may draw differently: a text renderer was added
    /// or removed, or found something new to draw.
    static TEXT_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// See [`TEXT_GENERATION`]; prepared message bodies are keyed by it.
pub fn text_generation() -> u64 {
    TEXT_GENERATION.get()
}

pub fn text_changed() {
    TEXT_GENERATION.set(TEXT_GENERATION.get() + 1);
}

/// Puts `item` in place of the one with its key, keeping its turn in
/// registration order, or adds it last. The key leaves every other kind,
/// so an id declared anew as another kind is only that.
macro_rules! put {
    ($registry:expr, $list:ident, $item:expr) => {{
        let item = $item;
        let key = item.key().to_owned();
        $registry.leave_others(&key, stringify!($list));
        match $registry
            .$list
            .iter()
            .position(|existing| existing.key() == key)
        {
            Some(at) => $registry.$list[at] = item,
            None => $registry.$list.push(item),
        }
    }};
}

impl Registry {
    /// Takes `key` out of every list but `keep`.
    fn leave_others(&mut self, key: &str, keep: &str) {
        if keep != "text" && self.text.iter().any(|item| item.key() == key) {
            self.text.retain(|item| item.key() != key);
            text_changed();
        }
        if keep != "accessories" {
            self.accessories.retain(|item| item.key() != key);
        }
        if keep != "messages" {
            self.messages.retain(|item| item.key() != key);
        }
        if keep != "links" {
            self.links.retain(|item| item.key() != key);
        }
        if keep != "inline" {
            self.inline.retain(|item| item.key() != key);
        }
        if keep != "attachments" {
            self.attachments.retain(|item| item.key() != key);
        }
        if keep != "actions" {
            self.actions.retain(|item| item.key() != key);
        }
    }
}

pub fn register_text(renderer: Rc<dyn TextRenderer>) {
    REGISTRY.with_borrow_mut(|r| put!(r, text, renderer));
    text_changed();
}

/// The pieces of `text` the text renderers draw, as `(range, key)`, in
/// order; where two overlap, the earlier registration wins.
pub fn text_matches(text: &str, message: InMessage) -> Vec<(std::ops::Range<usize>, String)> {
    let renderers = REGISTRY.with_borrow(|r| r.text.clone());
    if renderers.is_empty() || PLAIN.get() {
        return Vec::new();
    }
    let mut found: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for renderer in renderers.iter().filter(|r| on(r.key())) {
        let ranges =
            catch_unwind(AssertUnwindSafe(|| renderer.find(text, message))).unwrap_or_default();
        for range in ranges {
            let valid = range.start < range.end
                && range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end);
            if valid
                && !found
                    .iter()
                    .any(|(taken, _)| taken.start < range.end && range.start < taken.end)
            {
                found.push((range, renderer.key().to_owned()));
            }
        }
    }
    found.sort_by_key(|(range, _)| range.start);
    found
}

pub fn register_message_accessory(accessory: Rc<dyn MessageAccessory>) {
    REGISTRY.with_borrow_mut(|r| {
        r.accessories.push(accessory);
        r.accessories
            .sort_by(|a, b| a.order().cmp(&b.order()).then_with(|| a.key().cmp(b.key())));
    });
}

/// What the message accessories show beside `row`'s reactions.
/// Each with whether it comes before Agent Activity's own bubble
/// (`buzz.agent-activity/activity` at 0), by order then key.
pub fn message_accessories(
    row: &Arc<MessageRow>,
    open_thread: Option<&super::message::RowHandler>,
    find: Option<&super::message::FindRow>,
    cx: &App,
) -> Vec<(bool, AnyElement)> {
    let accessories = REGISTRY.with_borrow(|r| r.accessories.clone());
    accessories
        .into_iter()
        .filter(|accessory| on(accessory.key()))
        .filter_map(|accessory| {
            let before = (accessory.order(), accessory.key()) < (0, "buzz.agent-activity/activity");
            guarded(accessory.key(), || {
                accessory.render(row, open_thread, find, cx)
            })
            .map(|element| (before, element))
        })
        .collect()
}

/// Draws `matched` with text renderer `key`.
pub fn render_text(key: &str, matched: &str, message: &str, cx: &App) -> Option<AnyElement> {
    let renderer = REGISTRY.with_borrow(|r| r.text.iter().find(|r| r.key() == key).cloned())?;
    guarded(key, || renderer.render(matched, message, cx))
}

/// The bundled features: voice notes and custom emoji.
fn bundled() -> Registry {
    Registry {
        text: Vec::new(),
        accessories: Vec::new(),
        messages: Vec::new(),
        links: Vec::new(),
        inline: vec![Rc::new(Emoji)],
        attachments: vec![Rc::new(VoiceNotes)],
        actions: Vec::new(),
    }
}

/// Removes every renderer and action registered under `key`, as when the
/// plugin that added it stops.
pub fn unregister(key: &str) {
    REGISTRY.with_borrow_mut(|r| {
        if r.text.iter().any(|item| item.key() == key) {
            r.text.retain(|item| item.key() != key);
            text_changed();
        }
        r.messages.retain(|item| item.key() != key);
        r.accessories.retain(|item| item.key() != key);
        r.links.retain(|item| item.key() != key);
        r.inline.retain(|item| item.key() != key);
        r.attachments.retain(|item| item.key() != key);
        r.actions.retain(|item| item.key() != key);
    });
}

// Extension points: bundled features and plugins people added register.
pub fn register_message(renderer: Rc<dyn MessageRenderer>) {
    REGISTRY.with_borrow_mut(|r| put!(r, messages, renderer));
}

pub fn register_link(renderer: Rc<dyn LinkRenderer>) {
    REGISTRY.with_borrow_mut(|r| put!(r, links, renderer));
}

#[allow(dead_code)]
pub fn register_inline(renderer: Rc<dyn InlineRenderer>) {
    REGISTRY.with_borrow_mut(|r| r.inline.push(renderer));
}

pub fn register_attachment(renderer: Rc<dyn AttachmentRenderer>) {
    REGISTRY.with_borrow_mut(|r| put!(r, attachments, renderer));
}

pub fn register_action(action: Rc<dyn MessageAction>) {
    REGISTRY.with_borrow_mut(|r| put!(r, actions, action));
}

/// The actions that apply to `row`; a matcher that fails leaves the menu
/// usable.
pub fn actions(row: &MessageRow, viewer: &str) -> Vec<Rc<dyn MessageAction>> {
    let all = REGISTRY.with_borrow(|r| r.actions.clone());
    all.into_iter()
        .filter(|a| on(a.key()))
        .filter(|a| catch_unwind(AssertUnwindSafe(|| a.matches(row, viewer))).unwrap_or(false))
        .collect()
}

/// Runs the action `key` on `row`.
pub fn run_action(key: &str, row: &Arc<MessageRow>, window: &mut Window, cx: &mut App) {
    // The menu offered it for this row already.
    let found = REGISTRY.with_borrow(|r| r.actions.iter().find(|a| a.key() == key).cloned());
    if let Some(action) = found.filter(|a| on(a.key())) {
        let _ = guarded(action.key(), || {
            action.run(row, window, cx);
            Some(())
        });
    }
}

/// The byline markers actions draw for `row`.
pub fn markers(row: &MessageRow, viewer: &str, cx: &App) -> Vec<AnyElement> {
    actions(row, viewer)
        .into_iter()
        .filter_map(|a| guarded(a.key(), || a.marker(row, cx)))
        .collect()
}

/// Whether the plugin that contributed `key` (`plugin/id`) is on.
fn on(key: &str) -> bool {
    crate::plugins::enabled_key(key)
}

/// Runs a contribution; a panic counts as declining.
fn guarded<T>(key: &str, run: impl FnOnce() -> Option<T>) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(renderer = key, "a renderer failed; showing the default");
            None
        }
    }
}

/// `resolveMatch`: the lowest order among matching entries.
fn resolve<T: ?Sized>(
    entries: &[Rc<T>],
    matches: impl Fn(&T) -> bool,
    order: impl Fn(&T) -> i32,
    key: impl Fn(&T) -> String,
) -> Vec<Rc<T>> {
    let mut found: Vec<(i32, usize, Rc<T>)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| catch_unwind(AssertUnwindSafe(|| matches(e))).unwrap_or(false))
        .map(|(i, e)| (order(e), i, e.clone()))
        .collect();
    // Ties go to the lowest key (`MatchOrder`), not to whichever plugin
    // started first.
    found.sort_by(|(a, i, x), (b, j, y)| {
        a.cmp(b)
            .then_with(|| key(x).cmp(&key(y)))
            .then_with(|| i.cmp(j))
    });
    found.into_iter().map(|(_, _, e)| e).collect()
}

/// The winning message renderer's body for the whole row, if one draws
/// it (`MessageBody` sees the row as it is, time prefix and all).
pub fn custom_body(row: &Arc<MessageRow>, directory: &Directory, cx: &App) -> Option<AnyElement> {
    let candidates = REGISTRY.with_borrow(|r| {
        resolve(
            &r.messages,
            |m| on(m.key()) && m.matches(row),
            |m| m.order(row),
            |m| m.key().to_owned(),
        )
    });
    // The first that matches draws; one still waiting for its answer
    // leaves the body to Buzz until it comes.
    for renderer in candidates {
        if let Some(drawn) = guarded(renderer.key(), || renderer.render(row, directory, cx)) {
            return Some(drawn);
        }
        if renderer.waiting(row) {
            return None;
        }
    }
    None
}

thread_local! {
    /// What is drawn now is plain Markdown, no message's: no text or link
    /// renderer takes part (Projects bodies, agent replies).
    static PLAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Draws without text or link renderers, as plain Markdown does.
pub fn without_renderers<T>(draw: impl FnOnce() -> T) -> T {
    with_plain(true, draw)
}

/// Whether what is drawn now leaves the renderers out.
pub fn plain() -> bool {
    PLAIN.get()
}

/// Draws with the renderers left out or not, as `plain` says: what a
/// body drawn later, in layout, was drawn with.
pub fn with_plain<T>(plain: bool, draw: impl FnOnce() -> T) -> T {
    let before = PLAIN.replace(plain);
    let drawn = draw();
    PLAIN.set(before);
    drawn
}

/// A contributed link chip, when one outranks the bundled links.
pub fn link(url: &str, cx: &App) -> Option<AnyElement> {
    if PLAIN.get() {
        return None;
    }
    let candidates = REGISTRY.with_borrow(|r| {
        resolve(
            &r.links,
            |l| on(l.key()) && l.matches(url),
            |l| l.order(url),
            |l| l.key().to_owned(),
        )
    });
    // The bundled links renderer sits at 0 for known kinds, 100 for the
    // web; an equal order goes to the lower key, as everywhere.
    const BUNDLED: &str = "buzz.links/links";
    let bundled = match super::links::link_kind(url) {
        // Off, the Links plugin claims nothing.
        _ if !crate::plugins::enabled_key(BUNDLED) => i32::MAX,
        Some(super::links::LinkKind::Web) => 100,
        Some(_) => 0,
        // Buzz entity links, such as issues, are Links' at 0 too.
        None if url::Url::parse(url)
            .is_ok_and(|parsed| parsed.scheme() == "buzz" && parsed.host_str().is_some()) =>
        {
            0
        }
        None => i32::MAX,
    };
    // Only the one that ranks first, if it outranks Buzz's own: declining
    // (or not answered yet) leaves the chip to Buzz, not to the next.
    candidates
        .into_iter()
        .next()
        .filter(|renderer| {
            let order = renderer.order(url);
            order < bundled || (order == bundled && renderer.key() < BUNDLED)
        })
        .and_then(|renderer| guarded(renderer.key(), || renderer.render(url, cx)))
}

/// Inline text through the inline renderers, first that takes it.
pub fn inline(text: &str, emoji: &[CustomEmoji], media: &Media, cx: &App) -> Option<AnyElement> {
    let renderers = REGISTRY.with_borrow(|r| r.inline.clone());
    renderers
        .into_iter()
        .filter(|renderer| on(renderer.key()))
        .find_map(|renderer| guarded(renderer.key(), || renderer.render(text, emoji, media, cx)))
}

/// Every message contribution on now, by kind, as the conversation
/// readers list them: `(kind, key, title)`, the title for actions only.
pub fn listed() -> Vec<(&'static str, String, Option<String>)> {
    REGISTRY.with_borrow(|r| {
        let keyed = |kind: &'static str, key: &str| (kind, key.to_owned(), None);
        r.text
            .iter()
            .map(|it| keyed("inlineRenderers", it.key()))
            .chain(r.inline.iter().map(|it| keyed("inlineRenderers", it.key())))
            .chain(
                r.messages
                    .iter()
                    .map(|it| keyed("messageRenderers", it.key())),
            )
            .chain(r.links.iter().map(|it| keyed("linkRenderers", it.key())))
            .chain(
                r.attachments
                    .iter()
                    .map(|it| keyed("attachmentRenderers", it.key())),
            )
            .chain(
                r.accessories
                    .iter()
                    .map(|it| keyed("messageAccessories", it.key())),
            )
            .chain(r.actions.iter().map(|it| {
                (
                    "messageActions",
                    it.key().to_owned(),
                    Some(it.title().to_owned()),
                )
            }))
            .filter(|(_, key, _)| on(key))
            .collect()
    })
}

/// Whether an attachment renderer would draw `attachment`.
pub fn takes_attachment(attachment: &Attachment) -> bool {
    REGISTRY.with_borrow(|r| {
        r.attachments
            .iter()
            .any(|renderer| on(renderer.key()) && renderer.matches(attachment))
    })
}

/// Lets a renderer drawing the draft at `url` remove it.
pub fn removable(url: &str, remove: Remove) {
    REMOVERS.with_borrow_mut(|removers| removers.insert(url.to_owned(), remove));
}

thread_local! {
    /// Set while a view that offers no removal draws attachments.
    static NO_REMOVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `draw` for a view that offers no removal, such as the draft
/// viewer (no `onRemove`).
pub fn without_removal<T>(draw: impl FnOnce() -> T) -> T {
    let before = NO_REMOVAL.replace(true);
    let drawn = draw();
    NO_REMOVAL.set(before);
    drawn
}

/// Whether the attachments drawn now may offer removal.
pub fn removal_offered() -> bool {
    !NO_REMOVAL.get()
}

/// The draft at `url` can no longer be removed, as while it sends.
pub fn not_removable(url: &str) {
    REMOVERS.with_borrow_mut(|removers| removers.remove(url));
}

/// Whether the attachment at `url` is a draft a renderer may remove.
pub fn is_removable(url: &str) -> bool {
    REMOVERS.with_borrow(|removers| removers.contains_key(url))
}

/// Removes the draft at `url`; `false` when it is not one.
pub fn remove(url: &str, cx: &mut App) -> bool {
    let Some(remove) = REMOVERS.with_borrow_mut(|removers| removers.remove(url)) else {
        return false;
    };
    remove(cx);
    true
}

/// An attachment through the attachment renderers, if one takes it.
pub fn attachment(
    attachment: &Attachment,
    id: SharedString,
    media: &Media,
    cx: &App,
) -> Option<AnyElement> {
    let renderers = REGISTRY.with_borrow(|r| r.attachments.clone());
    let matching = renderers
        .into_iter()
        .filter(|renderer| on(renderer.key()) && renderer.matches(attachment));
    for renderer in matching {
        if let Some(drawn) = guarded(renderer.key(), || {
            renderer.render(attachment, id.clone(), media, cx)
        }) {
            return Some(drawn);
        }
        if renderer.waiting(attachment) {
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Bundled renderers

struct VoiceNotes;

impl AttachmentRenderer for VoiceNotes {
    fn key(&self) -> &str {
        "buzz.voice-notes/player"
    }

    fn matches(&self, attachment: &Attachment) -> bool {
        attachment.voice_note
    }

    fn render(
        &self,
        attachment: &Attachment,
        id: SharedString,
        media: &Media,
        cx: &App,
    ) -> Option<AnyElement> {
        Some(media::voice_note(id, attachment, media, cx))
    }
}

/// A lone `:shortcode:` naming one of the message's emoji, as an image.
struct Emoji;

impl InlineRenderer for Emoji {
    fn key(&self) -> &str {
        "buzz.emoji/custom"
    }

    fn render(
        &self,
        text: &str,
        emoji: &[CustomEmoji],
        media: &Media,
        _: &App,
    ) -> Option<AnyElement> {
        let code = text
            .strip_prefix(':')?
            .strip_suffix(':')?
            .to_ascii_lowercase();
        let entry = emoji.iter().find(|e| e.shortcode == code)?;
        Some(
            img(media::image(&entry.url, media))
                .size(px(14.))
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    struct Named(&'static str, i32);

    impl LinkRenderer for Named {
        fn key(&self) -> &str {
            self.0
        }
        fn order(&self, _: &str) -> i32 {
            self.1
        }
        fn matches(&self, url: &str) -> bool {
            url.starts_with("https://")
        }
        fn render(&self, _: &str, _: &App) -> Option<AnyElement> {
            None
        }
    }

    struct Broken;

    impl LinkRenderer for Broken {
        fn key(&self) -> &str {
            "broken"
        }
        fn order(&self, _: &str) -> i32 {
            -1
        }
        fn matches(&self, _: &str) -> bool {
            panic!("broken matcher")
        }
        fn render(&self, _: &str, _: &App) -> Option<AnyElement> {
            None
        }
    }

    #[test]
    fn lowest_order_wins_and_broken_matchers_drop_out() {
        let entries: Vec<Rc<dyn LinkRenderer>> = vec![
            Rc::new(Named("late", 5)),
            Rc::new(Broken),
            Rc::new(Named("early", 1)),
            Rc::new(Named("tie", 1)),
        ];
        let order = resolve(
            &entries,
            |l| l.matches("https://x"),
            |l| l.order("https://x"),
            |l| l.key().to_owned(),
        );
        let keys: Vec<&str> = order.iter().map(|l| l.key()).collect();
        assert_eq!(keys, ["early", "tie", "late"]);
        assert_eq!(guarded::<()>("x", || panic!("boom")), None);
    }
}
