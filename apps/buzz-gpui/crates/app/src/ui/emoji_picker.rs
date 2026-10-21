//! The emoji picker (ui.md §8.5, `EmojiPicker.tsx` over emoji-mart):
//! search, categories, frequently used, skin tones and the community's
//! custom emoji. The composer inserts what it picks; message rows react
//! with it. `mart` holds the emoji-mart behaviour it reproduces.
//!
//! Also here: emoji search shared with the composer's `:` completion
//! (`emoji-search.ts`), and the reaction usage counts behind the hover
//! bar's quick reactions (`quick-reactions.ts`).

mod mart;

use super::gif_picker::{self, GifPanel, PickedGif};
use super::media;
use crate::community::{AppState, Community};
use buzz_relay_client::messages::valid_reaction_content;
use buzz_relay_client::{CatalogStatus, CustomEmoji, EmojiSnapshot, Partition};
use buzz_ui::{
    ActiveTheme, Button, Icon, IconName, MaterialStyled, SearchField, Tab, TabVariant, Tabs,
    Tooltip, TypeStyled,
};
use emojis::{Emoji, Group, SkinTone, UnicodeVersion};
use gpui_kit::component::input::{
    Enter, Escape, InputEvent, InputState, MoveDown, MoveLeft, MoveRight, MoveUp,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use mart::{Arrow, Nav};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

/// The bundled font draws emoji through Unicode 16.
const NEWEST: UnicodeVersion = UnicodeVersion::new(16, 0);
/// `emojiButtonSize` and `emojiSize`.
const CELL: f32 = 48.;
const EMOJI_SIZE: f32 = 36.;
/// A category label: caption text with Mart's 4px padding.
const HEADER: f32 = 24.;
/// `--size-control`: the category bar.
const NAV: f32 = 40.;
/// `.emojiMart` is `min(21.75rem, 40vh)` tall: search, grid and bar.
const MART_HEIGHT: f32 = 348.;
/// The search field and its 12px margins.
const SEARCH: f32 = 64.;

/// What the user picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    Unicode(String),
    Custom(CustomEmoji),
}

impl Pick {
    /// The text it stands for: the emoji itself, or `:code:`.
    pub fn text(&self) -> String {
        match self {
            Pick::Unicode(text) => text.clone(),
            Pick::Custom(emoji) => format!(":{}:", emoji.shortcode),
        }
    }
}

pub struct Picked(pub Pick);

/// A plugin reaction picker chose one of the community's custom emoji by
/// `:shortcode:`, written as it chose to: the reaction says that text,
/// tagged with the catalog's emoji (`react(content, catalogEmoji(..))`).
pub struct PickedAs(pub String, pub CustomEmoji);

/// Escape in the picker: close it without a pick.
pub struct Dismissed;

// ---------------------------------------------------------------------------
// Search

/// One searchable emoji.
#[derive(Clone, Debug, PartialEq)]
pub enum Candidate {
    Unicode(&'static Emoji),
    Custom(CustomEmoji),
}

impl Candidate {
    pub fn shortcode(&self) -> &str {
        match self {
            Candidate::Unicode(emoji) => emoji.shortcode().unwrap_or_default(),
            Candidate::Custom(emoji) => &emoji.shortcode,
        }
    }

    pub fn pick(&self, tone: Option<SkinTone>) -> Pick {
        match self {
            Candidate::Unicode(emoji) => Pick::Unicode(toned(emoji, tone).as_str().to_string()),
            Candidate::Custom(emoji) => Pick::Custom(emoji.clone()),
        }
    }
}

fn toned(emoji: &'static Emoji, tone: Option<SkinTone>) -> &'static Emoji {
    tone.and_then(|tone| emoji.with_skin_tone(tone))
        .unwrap_or(emoji)
}

struct Indexed {
    emoji: &'static Emoji,
    /// Shortcodes, then the words of the name, lowercase.
    terms: Vec<String>,
}

static INDEX: LazyLock<Vec<Indexed>> = LazyLock::new(|| {
    emojis::iter()
        .filter(|emoji| emoji.unicode_version() <= NEWEST)
        .map(|emoji| Indexed {
            emoji,
            terms: emoji
                .shortcodes()
                .map(str::to_lowercase)
                .chain(
                    emoji
                        .name()
                        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
                        .filter(|word| !word.is_empty())
                        .map(str::to_lowercase),
                )
                .collect(),
        })
        .collect()
});

fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '-' | '_') && !c.is_whitespace())
        .collect()
}

/// `shortcodeScore`: exact, prefix, substring, then subsequence span.
fn shortcode_score(query: &str, shortcode: &str) -> Option<(u8, usize)> {
    let target = normalize(shortcode);
    if target == query {
        return Some((0, 0));
    }
    if target.starts_with(query) {
        return Some((1, 0));
    }
    if let Some(index) = target.find(query) {
        return Some((2, index));
    }
    let (mut first, mut last, mut wanted) = (None, 0, query.chars().peekable());
    for (index, c) in target.chars().enumerate() {
        if wanted.peek() == Some(&c) {
            wanted.next();
            first.get_or_insert(index);
            last = index;
        }
    }
    (wanted.peek().is_none()).then(|| (3, last - first.unwrap_or(0)))
}

/// `rankShortcodes`.
fn rank(query: &str, items: &[Candidate]) -> Vec<(Candidate, u8)> {
    let mut ranked: Vec<(Candidate, (u8, usize))> = items
        .iter()
        .filter_map(|item| Some((item.clone(), shortcode_score(query, item.shortcode())?)))
        .collect();
    ranked.sort_by(|(a, sa), (b, sb)| {
        sa.cmp(sb)
            .then(a.shortcode().len().cmp(&b.shortcode().len()))
            .then(a.shortcode().cmp(b.shortcode()))
    });
    ranked
        .into_iter()
        .map(|(item, (tier, _))| (item, tier))
        .collect()
}

/// `searchEmoji`: name and shortcode word matches, then fuzzy shortcode
/// matches, with exact and prefix shortcode matches first.
pub fn search(query: &str, custom: &[CustomEmoji], limit: usize) -> Vec<Candidate> {
    let needle = normalize(query);
    if needle.is_empty() {
        return Vec::new();
    }
    let lower = query.to_lowercase();
    let mut semantic: Vec<(usize, &Indexed)> = INDEX
        .iter()
        .filter_map(|entry| {
            entry
                .terms
                .iter()
                .enumerate()
                .filter(|(_, term)| term.starts_with(&lower))
                .map(|(index, term)| index * 2 + usize::from(*term != lower))
                .min()
                .map(|score| (score, entry))
        })
        .collect();
    semantic.sort_by(|(a, x), (b, y)| a.cmp(b).then(x.emoji.as_str().cmp(y.emoji.as_str())));
    let semantic: Vec<Candidate> = semantic
        .into_iter()
        .map(|(_, entry)| Candidate::Unicode(entry.emoji))
        .collect();
    let customs: Vec<Candidate> = custom.iter().cloned().map(Candidate::Custom).collect();
    let natives: Vec<Candidate> = INDEX
        .iter()
        .map(|entry| Candidate::Unicode(entry.emoji))
        .collect();
    let mut combined = semantic.clone();
    combined.extend(rank(&needle, &customs).into_iter().map(|(item, _)| item));
    combined.extend(
        rank(&needle, &natives)
            .into_iter()
            .map(|(item, _)| item)
            .filter(|item| !semantic.contains(item)),
    );
    let strong: Vec<Candidate> = rank(&needle, &combined)
        .into_iter()
        .filter(|(_, tier)| *tier <= 1)
        .map(|(item, _)| item)
        .collect();
    let mut ranked = strong.clone();
    ranked.extend(combined.into_iter().filter(|item| !strong.contains(item)));
    ranked.truncate(limit);
    ranked
}

// ---------------------------------------------------------------------------
// Usage

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Used {
    emoji: String,
    count: u64,
    used: u64,
}

/// Reaction counts per community, saved in its `quick-reactions` pref,
/// and the shortcuts each community's message rows show.
#[derive(Default)]
struct Usage {
    entries: HashMap<Partition, Vec<Used>>,
    frozen: HashMap<Partition, Frozen>,
}

/// Shortcuts stay put while a channel is open (`useQuickReactions`), so
/// the bar does not reorder under the pointer right after a reaction.
struct Frozen {
    channel: Option<String>,
    catalog: Arc<[CustomEmoji]>,
    quick: Vec<SharedString>,
}

impl Global for Usage {}

const USAGE_PREF: &str = "quick-reactions";
pub const QUICK_DEFAULTS: [&str; 3] = ["👍", "❤️", "😂"];
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE: f64 = 9_007_199_254_740_991.;

/// The saved counts, keeping only well-formed entries (`read`): reaction
/// content the relay accepts, a positive whole count and a time.
fn read_usage(json: &str) -> Vec<Used> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    let mut entries: Vec<Used> = items
        .iter()
        .filter_map(|item| {
            let emoji = item.get("emoji")?.as_str()?;
            let count = item.get("count")?.as_f64()?;
            let used = item.get("used")?.as_f64()?;
            let valid = !emoji.trim().is_empty()
                && valid_reaction_content(emoji)
                && count.fract() == 0.
                && count > 0.
                && count <= MAX_SAFE
                && used >= 0.;
            valid.then(|| Used {
                emoji: emoji.to_string(),
                count: count as u64,
                used: used as u64,
            })
        })
        .collect();
    entries.sort_by(|a, b| b.count.cmp(&a.count).then(b.used.cmp(&a.used)));
    entries.truncate(24);
    entries
}

fn usage<'a>(partition: &Partition, cx: &'a mut App) -> &'a mut Vec<Used> {
    if !cx.has_global::<Usage>() {
        cx.set_global(Usage::default());
    }
    let stored = (!cx.global::<Usage>().entries.contains_key(partition)).then(|| {
        cx.global::<AppState>()
            .store
            .partition_pref(partition, USAGE_PREF)
            .ok()
            .flatten()
            .map(|json| read_usage(&json))
            .unwrap_or_default()
    });
    let entries = cx
        .global_mut::<Usage>()
        .entries
        .entry(partition.clone())
        .or_default();
    if let Some(stored) = stored {
        *entries = stored;
    }
    entries
}

/// Most used first, at most `limit`.
pub(crate) fn most_used(partition: &Partition, limit: usize, cx: &mut App) -> Vec<String> {
    let mut entries = usage(partition, cx).clone();
    entries.sort_by(|a, b| b.count.cmp(&a.count).then(b.used.cmp(&a.used)));
    entries
        .into_iter()
        .take(limit)
        .map(|entry| entry.emoji)
        .collect()
}

/// The hover bar's three reactions: the most used, then the defaults.
/// Custom ones stay only while the palette still has them.
pub fn quick_reactions(
    partition: &Partition,
    emoji: &EmojiSnapshot,
    cx: &mut App,
) -> Vec<SharedString> {
    let mut quick: Vec<SharedString> = Vec::new();
    for value in most_used(partition, 24, cx)
        .into_iter()
        .chain(QUICK_DEFAULTS.iter().map(|s| s.to_string()))
    {
        let custom_gone = value
            .strip_prefix(':')
            .and_then(|code| code.strip_suffix(':'))
            .is_some_and(|code| emoji.get(&code.to_lowercase()).is_none());
        if !custom_gone && !quick.iter().any(|q| q.as_ref() == value) {
            quick.push(value.into());
        }
        if quick.len() == 3 {
            break;
        }
    }
    quick
}

/// Counts one reaction (`recordReaction`).
pub fn record_reaction(partition: &Partition, emoji: &str, cx: &mut App) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let entries = usage(partition, cx);
    match entries.iter_mut().find(|entry| entry.emoji == emoji) {
        Some(entry) => {
            entry.count += 1;
            entry.used = now;
        }
        None => entries.push(Used {
            emoji: emoji.to_string(),
            count: 1,
            used: now,
        }),
    }
    entries.sort_by(|a, b| b.count.cmp(&a.count).then(b.used.cmp(&a.used)));
    entries.truncate(24);
    let json = serde_json::to_string(&entries).expect("usage serializes");
    if let Err(error) = cx
        .global::<AppState>()
        .store
        .set_partition_pref(partition, USAGE_PREF, &json)
    {
        tracing::warn!(%error, "could not save reaction usage");
    }
}

/// Forgets a community's reaction history once the viewer has left it
/// (`forgetQuickReactions`). The store's purge removes the saved copy.
pub fn forget_quick_reactions(partition: &Partition, cx: &mut App) {
    if cx.has_global::<Usage>() {
        let usage = cx.global_mut::<Usage>();
        usage.entries.remove(partition);
        usage.frozen.remove(partition);
    }
}

/// One hover-bar reaction: an emoji, or a custom one's image.
#[derive(Clone, Debug, PartialEq)]
pub struct QuickReaction {
    pub content: SharedString,
    pub image: Option<String>,
}

/// The quick reactions as message rows show them. They are worked out
/// once per channel visit and palette, then kept.
pub fn quick_bar(community: &Entity<Community>, cx: &mut App) -> Vec<QuickReaction> {
    let (partition, emoji, channel) = {
        let community = community.read(cx);
        (
            community.partition(),
            community.snapshot.emoji.clone(),
            community.reading.clone(),
        )
    };
    if !cx.has_global::<Usage>() {
        cx.set_global(Usage::default());
    }
    let frozen = cx
        .global::<Usage>()
        .frozen
        .get(&partition)
        .filter(|frozen| frozen.channel == channel && Arc::ptr_eq(&frozen.catalog, &emoji.entries))
        .map(|frozen| frozen.quick.clone());
    let quick = match frozen {
        Some(quick) => quick,
        None => {
            let quick = quick_reactions(&partition, &emoji, cx);
            cx.global_mut::<Usage>().frozen.insert(
                partition,
                Frozen {
                    channel,
                    catalog: emoji.entries.clone(),
                    quick: quick.clone(),
                },
            );
            quick
        }
    };
    quick
        .into_iter()
        .map(|content| {
            let image = content
                .strip_prefix(':')
                .and_then(|code| code.strip_suffix(':'))
                .and_then(|code| emoji.get(&code.to_lowercase()))
                .map(|custom| custom.url.clone());
            QuickReaction { content, image }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// emoji-mart's device storage

/// What emoji-mart keeps in local storage: the pick history behind
/// "Frequently used" and the default skin tone (1 to 6).
struct MartStore {
    frequent: mart::Frequent,
    skin: usize,
}

impl Global for MartStore {}

const FREQUENTLY_PREF: &str = "emoji-mart.frequently";
const LAST_PREF: &str = "emoji-mart.last";
const SKIN_PREF: &str = "emoji-mart.skin";

fn mart_store(cx: &mut App) -> &mut MartStore {
    if !cx.has_global::<MartStore>() {
        let store = &cx.global::<AppState>().store;
        let pref = |name: &str| store.device_pref(name).ok().flatten();
        // An empty history would hide the defaults, so it counts as none.
        let index = pref(FREQUENTLY_PREF)
            .and_then(|json| serde_json::from_str::<BTreeMap<String, u64>>(&json).ok())
            .filter(|index| !index.is_empty());
        let last = index
            .as_ref()
            .and(pref(LAST_PREF))
            .and_then(|json| serde_json::from_str::<String>(&json).ok());
        let skin = pref(SKIN_PREF)
            .and_then(|json| json.parse::<usize>().ok())
            .filter(|skin| (1..=TONES.len()).contains(skin))
            .unwrap_or(1);
        cx.set_global(MartStore {
            frequent: mart::Frequent { index, last },
            skin,
        });
    }
    cx.global_mut::<MartStore>()
}

fn save_frequent(cx: &mut App) {
    let frequent = mart_store(cx).frequent.clone();
    let store = &cx.global::<AppState>().store;
    let index = serde_json::to_string(&frequent.index.unwrap_or_default()).expect("serializes");
    let mut saved = store.set_device_pref(FREQUENTLY_PREF, &index);
    if let Some(last) = &frequent.last {
        saved = saved.and_then(|()| {
            store.set_device_pref(LAST_PREF, &serde_json::to_string(last).expect("serializes"))
        });
    }
    if let Err(error) = saved {
        tracing::warn!(%error, "could not save the emoji history");
    }
}

fn set_skin(skin: usize, cx: &mut App) {
    mart_store(cx).skin = skin;
    if let Err(error) = cx
        .global::<AppState>()
        .store
        .set_device_pref(SKIN_PREF, &skin.to_string())
    {
        tracing::warn!(%error, "could not save the skin tone");
    }
}

/// The scope in a custom emoji's Mart id.
fn scope(partition: &Partition) -> String {
    format!("{}|{}", partition.origin, partition.viewer)
}

// ---------------------------------------------------------------------------
// The picker

#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Frequent,
    People,
    Nature,
    Food,
    Activity,
    Travel,
    Objects,
    Symbols,
    Flags,
    Custom,
}

impl Category {
    const ALL: [Category; 10] = [
        Category::Frequent,
        Category::People,
        Category::Nature,
        Category::Food,
        Category::Activity,
        Category::Travel,
        Category::Objects,
        Category::Symbols,
        Category::Flags,
        Category::Custom,
    ];

    fn title(self) -> &'static str {
        match self {
            Category::Frequent => "Frequently used",
            Category::People => "Smileys & People",
            Category::Nature => "Animals & Nature",
            Category::Food => "Food & Drink",
            Category::Activity => "Activity",
            Category::Travel => "Travel & Places",
            Category::Objects => "Objects",
            Category::Symbols => "Symbols",
            Category::Flags => "Flags",
            Category::Custom => "Custom",
        }
    }

    /// buzz-app's `pickerIcons` (`icons/svg.ts`): its "orange" is Tabler
    /// `lemon-2` and its "shapes" is Tabler `category`.
    fn icon(self) -> IconName {
        match self {
            Category::Frequent => IconName::Clock,
            Category::People => IconName::Smiley,
            Category::Nature => IconName::Paw,
            Category::Food => IconName::Lemon,
            Category::Activity => IconName::Barbell,
            Category::Travel => IconName::Car,
            Category::Objects => IconName::Bulb,
            Category::Symbols => IconName::Category,
            Category::Flags => IconName::Flag,
            Category::Custom => IconName::Asterisk,
        }
    }

    fn groups(self) -> &'static [Group] {
        match self {
            Category::People => &[Group::SmileysAndEmotion, Group::PeopleAndBody],
            Category::Nature => &[Group::AnimalsAndNature],
            Category::Food => &[Group::FoodAndDrink],
            Category::Activity => &[Group::Activities],
            Category::Travel => &[Group::TravelAndPlaces],
            Category::Objects => &[Group::Objects],
            Category::Symbols => &[Group::Symbols],
            Category::Flags => &[Group::Flags],
            Category::Frequent | Category::Custom => &[],
        }
    }
}

/// Mart's six skins: the tone, its label and its swatch color.
const TONES: [(Option<SkinTone>, &str, u32); 6] = [
    (None, "Default", 0xffc93a),
    (Some(SkinTone::Light), "Light", 0xffdab7),
    (Some(SkinTone::MediumLight), "Medium-Light", 0xe7b98f),
    (Some(SkinTone::Medium), "Medium", 0xc88c61),
    (Some(SkinTone::MediumDark), "Medium-Dark", 0xa46134),
    (Some(SkinTone::Dark), "Dark", 0x5d4437),
];
const CHOOSE_TONE: &str = "Choose default skin tone";

#[derive(Clone)]
enum Row {
    Header(&'static str),
    Cells {
        /// Its line in the keyboard grid.
        line: usize,
        cells: Vec<Candidate>,
        /// Category lines keep empty slots, so a short last line stays
        /// aligned; search lines do not.
        pad: bool,
    },
}

impl Row {
    fn height(&self) -> f32 {
        match self {
            Row::Header(_) => HEADER,
            Row::Cells { .. } => CELL,
        }
    }
}

pub struct EmojiPicker {
    community: Entity<Community>,
    search: Entity<InputState>,
    list: ListState,
    rows: Rc<Vec<Row>>,
    /// Where each row starts, for the sticky label and keyboard scrolling.
    tops: Vec<f32>,
    /// Each label's row.
    headers: Vec<(usize, &'static str)>,
    /// Where each shown category starts.
    sections: Vec<(Category, usize)>,
    /// The category bar, which stays while searching.
    nav: Vec<Category>,
    /// The keyboard grid: each line's row and length.
    lines: Vec<(usize, usize)>,
    /// The selected (line, column), from the keyboard or the pointer.
    selected: Option<(usize, usize)>,
    searching: bool,
    active: Category,
    per_line: usize,
    /// The composer's width, which a composer picker fits; a reaction
    /// picker fits the window.
    container: Option<f32>,
    grid_height: f32,
    tones_open: bool,
    /// The tone a hovered menu option previews.
    preview_tone: Option<usize>,
    /// The palette the rows were built from.
    built_for: Option<Arc<[CustomEmoji]>>,
    /// The composer's picker also offers GIFs, when the relay does.
    gifs: Option<GifTab>,
    /// Whether it picks reactions, so a plugin's picker may replace it.
    reactions: bool,
    /// The plugin reaction picker shown instead, by its key.
    plugin_view: Option<(&'static str, AnyView)>,
    _subscriptions: Vec<Subscription>,
}

struct GifTab {
    panel: Entity<GifPanel>,
    /// `None` until the relay's NIP-11 answers.
    available: Option<bool>,
    showing: bool,
    _check: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<Picked> for EmojiPicker {}
impl EventEmitter<PickedAs> for EmojiPicker {}
impl EventEmitter<PickedGif> for EmojiPicker {}
impl EventEmitter<Dismissed> for EmojiPicker {}

impl EmojiPicker {
    pub fn new(community: Entity<Community>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search emoji"));
        let list = ListState::new(0, ListAlignment::Top, px(400.));
        let weak = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _, cx| {
            let first = event.visible_range.start;
            let _ = weak.update(cx, |this, cx| {
                if let Some(active) = this
                    .sections
                    .iter()
                    .rev()
                    .find(|(_, start)| *start <= first)
                    .map(|(category, _)| *category)
                {
                    this.active = active;
                }
                // The sticky label follows the scroll.
                cx.notify();
            });
        });
        let subscriptions = vec![
            cx.subscribe(&search, |this, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.rebuild(cx);
                    // Mart selects the first result while the caret is at
                    // the end, so Enter picks it.
                    let state = state.read(cx);
                    let at_end = state.selected_range().start == state.value().len();
                    this.selected =
                        (this.searching && at_end && !this.lines.is_empty()).then_some((0, 0));
                    cx.notify();
                }
            }),
            cx.observe(&community, |this, _, cx| {
                let custom = this.community.read(cx).snapshot.emoji.entries.clone();
                if this
                    .built_for
                    .as_ref()
                    .is_none_or(|built| !Arc::ptr_eq(built, &custom))
                {
                    this.rebuild(cx);
                    cx.notify();
                }
            }),
        ];
        let mut picker = Self {
            community,
            search,
            list,
            rows: Rc::default(),
            tops: Vec::new(),
            headers: Vec::new(),
            sections: Vec::new(),
            nav: Vec::new(),
            lines: Vec::new(),
            selected: None,
            searching: false,
            active: Category::Frequent,
            per_line: 6,
            container: None,
            grid_height: MART_HEIGHT - SEARCH - NAV,
            tones_open: false,
            preview_tone: None,
            built_for: None,
            gifs: None,
            reactions: false,
            plugin_view: None,
            _subscriptions: subscriptions,
        };
        picker.rebuild(cx);
        picker
    }

    /// Clears the search and scrolls to the top, for a fresh open.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.tones_open = false;
        self.preview_tone = None;
        if let Some(gifs) = &mut self.gifs {
            gifs.showing = false;
        }
        self.check_gifs(cx);
        self.rebuild(cx);
        cx.notify();
    }

    /// Adds the GIF tab, shown once the relay says it offers GIF search.
    pub fn enable_gifs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.new(|cx| GifPanel::new(self.community.clone(), window, cx));
        let subscription = cx.subscribe(&panel, |_, _, PickedGif(markdown), cx| {
            cx.emit(PickedGif(markdown.clone()))
        });
        self.gifs = Some(GifTab {
            panel,
            available: None,
            showing: false,
            _check: None,
            _subscription: subscription,
        });
        self.check_gifs(cx);
    }

    /// Asks the relay again unless it already said yes; a "no" or a
    /// failure is not remembered, as a relay can turn GIFs on.
    fn check_gifs(&mut self, cx: &mut Context<Self>) {
        let Some(gifs) = &mut self.gifs else {
            return;
        };
        if gifs.available == Some(true) || gifs._check.is_some() {
            return;
        }
        let community = self.community.clone();
        gifs._check = Some(cx.spawn(async move |this, cx| {
            let available = gif_picker::available(&community, cx).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(gifs) = &mut this.gifs {
                    gifs.available = Some(available);
                    gifs._check = None;
                    cx.notify();
                }
            });
        }));
    }

    /// Switches tabs, carrying the search query across both ways.
    fn show_gifs(&mut self, showing: bool, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_string();
        let Some(gifs) = &mut self.gifs else {
            return;
        };
        if gifs.showing == showing {
            return;
        }
        gifs.showing = showing;
        let panel = gifs.panel.clone();
        if showing {
            panel.update(cx, |panel, cx| panel.open(&query, window, cx));
        } else {
            let query = panel.read(cx).query(cx);
            self.search
                .update(cx, |state, cx| state.set_value(query, window, cx));
            self.rebuild(cx);
            self.focus(window, cx);
        }
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.search.update(cx, |state, cx| state.focus(window, cx));
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let community = self.community.read(cx);
        let custom = community.snapshot.emoji.entries.clone();
        let scope = scope(&community.partition());
        self.built_for = Some(custom.clone());
        let query = self.search.read(cx).value().to_string();
        let per_line = self.per_line;
        let mut rows = Vec::new();
        self.sections.clear();
        self.headers.clear();
        self.lines.clear();
        let mut push = |rows: &mut Vec<Row>, cells: Vec<Candidate>, pad: bool| {
            for chunk in cells.chunks(per_line) {
                self.lines.push((rows.len(), chunk.len()));
                rows.push(Row::Cells {
                    line: self.lines.len() - 1,
                    cells: chunk.to_vec(),
                    pad,
                });
            }
        };
        let (frequent, save) = mart_store(cx).frequent.get(mart::FREQUENT_ROWS, per_line);
        if save {
            save_frequent(cx);
        }
        let frequent: Vec<Candidate> = frequent
            .iter()
            .filter_map(|id| frequent_candidate(id, &scope, &custom))
            .collect();
        self.nav = Category::ALL
            .into_iter()
            .filter(|category| match category {
                Category::Frequent => !frequent.is_empty(),
                Category::Custom => !custom.is_empty(),
                _ => true,
            })
            .collect();
        let found = mart::search(&query, &scope, &custom, mart::MAX_RESULTS);
        self.searching = found.is_some();
        if let Some(found) = found {
            self.headers.push((0, "Search Results"));
            rows.push(Row::Header("Search Results"));
            push(&mut rows, found, false);
        } else {
            for category in self.nav.clone() {
                let cells: Vec<Candidate> = match category {
                    Category::Frequent => frequent.clone(),
                    Category::Custom => custom.iter().cloned().map(Candidate::Custom).collect(),
                    _ => INDEX
                        .iter()
                        .filter(|entry| category.groups().contains(&entry.emoji.group()))
                        .map(|entry| Candidate::Unicode(entry.emoji))
                        .collect(),
                };
                if cells.is_empty() {
                    continue;
                }
                self.sections.push((category, rows.len()));
                self.headers.push((rows.len(), category.title()));
                rows.push(Row::Header(category.title()));
                push(&mut rows, cells, true);
            }
        }
        let mut top = 0.;
        self.tops = rows
            .iter()
            .map(|row| {
                let this = top;
                top += row.height();
                this
            })
            .collect();
        self.rows = Rc::new(rows);
        self.list.reset(self.rows.len());
        self.active = Category::Frequent;
        self.selected = None;
    }

    fn jump(&mut self, category: Category, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.read(cx).value().is_empty() {
            self.search
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.rebuild(cx);
        }
        if let Some((_, row)) = self.sections.iter().find(|(c, _)| *c == category) {
            self.list.scroll_to(ListOffset {
                item_ix: *row,
                offset_in_item: px(0.),
            });
            self.active = category;
            cx.notify();
        }
    }

    /// The tone picks use (Mart's `skin`).
    fn tone(&self, cx: &mut App) -> Option<SkinTone> {
        TONES[mart_store(cx).skin - 1].0
    }

    /// The tone the grid shows: a hovered menu option previews its own.
    fn shown_skin(&self, cx: &mut App) -> usize {
        self.preview_tone.unwrap_or_else(|| mart_store(cx).skin)
    }

    /// Counts the pick in "Frequently used" (`FrequentlyUsed.add`) and
    /// hands it out.
    fn pick(&mut self, candidate: &Candidate, cx: &mut Context<Self>) {
        let id = match candidate {
            Candidate::Unicode(emoji) => mart::id_of(emoji)
                .map(str::to_string)
                .unwrap_or_else(|| emoji.as_str().to_string()),
            Candidate::Custom(emoji) => {
                let partition = self.community.read(cx).partition();
                mart::custom_id(&scope(&partition), &emoji.shortcode)
            }
        };
        mart_store(cx).frequent.add(&id);
        save_frequent(cx);
        let tone = self.tone(cx);
        cx.emit(Picked(candidate.pick(tone)));
    }

    fn selected_candidate(&self) -> Option<Candidate> {
        let (line, column) = self.selected?;
        match self.rows.get(self.lines.get(line)?.0)? {
            Row::Cells { cells, .. } => cells.get(column).cloned(),
            Row::Header(_) => None,
        }
    }

    fn search_focused(&self, window: &Window, cx: &App) -> bool {
        self.gifs.as_ref().is_none_or(|gifs| !gifs.showing)
            && self.search.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Arrow keys from the search field (Mart's `handleSearchKeyDown`).
    fn arrow(&mut self, arrow: Arrow, window: &Window, cx: &mut Context<Self>) {
        if !self.search_focused(window, cx) {
            return;
        }
        let at_end = {
            let state = self.search.read(cx);
            state.selected_range().start == state.value().len()
        };
        let lengths: Vec<usize> = self.lines.iter().map(|(_, length)| *length).collect();
        match mart::navigate(&lengths, self.selected, arrow, at_end) {
            Nav::Ignore => {}
            Nav::Clear => {
                if self.selected.take().is_some() {
                    cx.notify();
                }
            }
            Nav::Select(line, column) => {
                self.selected = Some((line, column));
                self.reveal(line);
                cx.stop_propagation();
                cx.notify();
            }
        }
    }

    fn scroll_y(&self) -> f32 {
        let top = self.list.logical_scroll_top();
        self.tops.get(top.item_ix).copied().unwrap_or(0.) + f32::from(top.offset_in_item)
    }

    fn scroll_to_y(&self, y: f32) {
        let item_ix = self.tops.partition_point(|top| *top <= y).saturating_sub(1);
        let offset = y - self.tops.get(item_ix).copied().unwrap_or(0.);
        self.list.scroll_to(ListOffset {
            item_ix,
            offset_in_item: px(offset.max(0.)),
        });
    }

    /// Mart's `scrollTo({ row })`: the first line scrolls to the top;
    /// another scrolls just enough to show it below the sticky label,
    /// with most of the next line too.
    fn reveal(&self, line: usize) {
        let y = if line == 0 {
            0.
        } else {
            let Some(top) = self
                .lines
                .get(line)
                .and_then(|(row, _)| self.tops.get(*row))
            else {
                return;
            };
            let top = top - HEADER + 1.;
            let bottom = top + CELL + CELL * 0.88;
            let scroll = self.scroll_y();
            if top < scroll {
                top
            } else if bottom > scroll + self.grid_height {
                bottom - self.grid_height
            } else {
                return;
            }
        };
        self.scroll_to_y(y);
    }

    /// The label pinned over the grid: the current section's, pushed up
    /// by the next one as it arrives.
    fn sticky(&self) -> Option<(&'static str, f32)> {
        let y = self.scroll_y();
        let current = self
            .headers
            .iter()
            .rposition(|(row, _)| self.tops[*row] <= y)?;
        let offset = self
            .headers
            .get(current + 1)
            .map_or(0., |(row, _)| (self.tops[*row] - y - HEADER).min(0.));
        Some((self.headers[current].1, offset))
    }
}

/// A "Frequently used" id as an emoji: Mart's own, one of this
/// community's custom emoji, or a newer emoji kept by its text.
fn frequent_candidate(id: &str, scope: &str, custom: &[CustomEmoji]) -> Option<Candidate> {
    if let Some(code) = id.strip_prefix(&mart::custom_id(scope, "")) {
        return custom
            .iter()
            .find(|emoji| emoji.shortcode == code)
            .cloned()
            .map(Candidate::Custom);
    }
    mart::by_id(id)
        .or_else(|| {
            (!id.starts_with("buzz-custom/"))
                .then(|| emojis::get(id))
                .flatten()
        })
        .map(Candidate::Unicode)
}

fn header(title: &'static str, cx: &App) -> Div {
    div()
        .h(px(HEADER))
        .flex()
        .items_center()
        .px(px(16.))
        .text_caption()
        .text_color(cx.colors().text_subtle)
        .child(title)
}

fn swatch(skin: usize) -> Div {
    div()
        .size(px(16.))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(hsla(0., 0., 0., 0.3))
        .bg(rgb(TONES[skin - 1].2))
}

fn cell(
    candidate: &Candidate,
    at: (usize, usize),
    selected: bool,
    tone: Option<SkinTone>,
    picker: WeakEntity<EmojiPicker>,
    community: &Entity<Community>,
    cx: &App,
) -> AnyElement {
    let c = cx.colors();
    let label: SharedString = match candidate {
        Candidate::Unicode(emoji) if emoji.shortcode().is_none() => emoji.name().into(),
        _ => format!(":{}:", candidate.shortcode()).into(),
    };
    let glyph = match candidate {
        Candidate::Unicode(emoji) => div()
            .text_size(px(EMOJI_SIZE))
            .line_height(px(CELL))
            .child(toned(emoji, tone).as_str())
            .into_any_element(),
        Candidate::Custom(emoji) => {
            img(media::image(&emoji.url, community.read(cx).session.media()))
                .size(px(32.))
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        }
    };
    let (hover, click) = (picker.clone(), candidate.clone());
    div()
        .id(SharedString::from(format!("emoji-{}-{}", at.0, at.1)))
        .size(px(CELL))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .when(selected, |d| d.bg(c.border_standard))
        .tooltip(Tooltip::text(label))
        .child(glyph)
        // Mart's selection follows the pointer too.
        .on_hover(move |hovered, _, cx| {
            let _ = hover.update(cx, |this, cx| {
                if this.tones_open {
                    return;
                }
                if *hovered && this.selected != Some(at) {
                    this.selected = Some(at);
                    cx.notify();
                } else if !*hovered && this.selected == Some(at) {
                    this.selected = None;
                    cx.notify();
                }
            });
        })
        .on_click(move |_, _, cx| {
            let _ = picker.update(cx, |this, cx| this.pick(&click, cx));
        })
        .into_any_element()
}

impl EmojiPicker {
    /// Fits the picker to its composer's width. True when it changed.
    pub fn fit(&mut self, width: f32) -> bool {
        let changed = self.container.is_none_or(|old| (old - width).abs() > 0.5);
        if changed {
            self.container = Some(width);
        }
        changed
    }
}

impl EmojiPicker {
    fn render_grid(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let available = self
            .container
            .unwrap_or_else(|| f32::from(window.viewport_size().width));
        let per_line = mart::per_line(available);
        if per_line != self.per_line {
            self.per_line = per_line;
            self.rebuild(cx);
        }
        let width = mart::width(per_line, available);
        // Ten category buttons do not fit a narrow picker.
        let nav_shown = per_line >= 6;
        let tabs = self
            .gifs
            .as_ref()
            .filter(|gifs| gifs.available == Some(true))
            .map(|gifs| (gifs.showing, gifs.panel.clone()));
        let height = MART_HEIGHT.min(f32::from(window.viewport_size().height) * 0.4);
        // Under tabs, the search's top margin is the tabs' gap instead.
        self.grid_height = (height - SEARCH - if nav_shown { NAV } else { 0. }
            + if tabs.is_some() { 12. } else { 0. })
        .max(CELL);

        let rows = self.rows.clone();
        let tone = TONES[self.shown_skin(cx) - 1].0;
        let selected = self.selected;
        let weak = cx.entity().downgrade();
        let community = self.community.clone();
        let grid = list(self.list.clone(), move |index, _, cx| {
            let Some(row) = rows.get(index) else {
                return div().into_any_element();
            };
            match row {
                Row::Header(title) => header(title, cx).into_any_element(),
                Row::Cells { line, cells, pad } => div()
                    .w_full()
                    .h(px(CELL))
                    .flex()
                    .justify_between()
                    .px(px(12.))
                    .children(cells.iter().enumerate().map(|(column, candidate)| {
                        let at = (*line, column);
                        cell(
                            candidate,
                            at,
                            selected == Some(at),
                            tone,
                            weak.clone(),
                            &community,
                            cx,
                        )
                    }))
                    .when(*pad, |d| {
                        d.children(
                            (cells.len()..per_line).map(|_| div().size(px(CELL)).flex_none()),
                        )
                    })
                    .into_any_element(),
            }
        })
        .size_full();
        let sticky = self.sticky().map(|(title, offset)| {
            header(title, cx)
                .absolute()
                .left_0()
                .right_0()
                .top(px(offset))
                .bg(c.surface_popover)
        });
        let grid = div()
            .relative()
            .h(px(self.grid_height))
            .w_full()
            .overflow_hidden()
            .child(grid)
            .children(sticky);

        let nav = nav_shown.then(|| {
            div()
                .flex()
                .items_center()
                .px(px(8.))
                .h(px(NAV))
                .flex_none()
                .border_t_1()
                .border_color(c.border_standard)
                .children(self.nav.iter().map(|category| {
                    let category = *category;
                    // Mart's bar shows no category while searching.
                    let active = !self.searching && category == self.active;
                    div()
                        .id(SharedString::from(format!(
                            "emoji-nav-{}",
                            category.title()
                        )))
                        .flex_1()
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .text_color(if active {
                            c.text_standard
                        } else {
                            c.text_subtle
                        })
                        .hover(|s| s.text_color(c.text_standard))
                        .tooltip(Tooltip::text(category.title()))
                        .child(
                            div()
                                .size(px(28.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .when(active, |d| d.bg(c.affordance_subtle_hover))
                                .child(Icon::new(category.icon()).size(18.)),
                        )
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.jump(category, window, cx)),
                        )
                }))
                .child(self.tone_button(true, cx))
        });

        let status = match &self.community.read(cx).snapshot.emoji.status {
            CatalogStatus::Error(error) => Some(error.clone()),
            _ => None,
        };
        let session = self.community.read(cx).session.clone();
        let emoji_tab = !matches!(tabs, Some((true, _)));
        div()
            .id("emoji-picker")
            .relative()
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(Dismissed);
            }))
            .capture_action(
                cx.listener(|this, _: &MoveLeft, window, cx| this.arrow(Arrow::Left, window, cx)),
            )
            .capture_action(
                cx.listener(|this, _: &MoveRight, window, cx| this.arrow(Arrow::Right, window, cx)),
            )
            .capture_action(
                cx.listener(|this, _: &MoveUp, window, cx| this.arrow(Arrow::Up, window, cx)),
            )
            .capture_action(
                cx.listener(|this, _: &MoveDown, window, cx| this.arrow(Arrow::Down, window, cx)),
            )
            .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                if this.search_focused(window, cx) {
                    cx.stop_propagation();
                    if let Some(candidate) = this.selected_candidate() {
                        this.pick(&candidate, cx);
                    }
                }
            }))
            .w(px(width))
            .flex()
            .flex_col()
            .floating_surface(&c)
            .overflow_hidden()
            .occlude()
            .text_color(c.text_standard)
            .when_some(tabs.clone(), |d, (showing, _)| {
                // `Tabs variant="panel" label="Media type"`.
                d.child(
                    div().mx(px(12.)).mt(px(12.)).child(
                        Tabs::new(TabVariant::Panel)
                            .tab(
                                Tab::new("picker-emoji", "Emoji")
                                    .selected(!showing)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_gifs(false, window, cx)
                                    })),
                            )
                            .tab(Tab::new("picker-gif", "GIF").selected(showing).on_click(
                                cx.listener(|this, _, window, cx| this.show_gifs(true, window, cx)),
                            )),
                    ),
                )
            })
            .map(|d| match tabs {
                Some((true, panel)) => {
                    panel.update(cx, |panel, _| panel.set_width(width - 2.));
                    d.child(div().pt(px(12.)).child(panel))
                }
                _ => d
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .px(px(12.))
                            .py(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .child(SearchField::new(&self.search).capsule()),
                            )
                            .when(!nav_shown, |d| d.child(self.tone_button(false, cx))),
                    )
                    .child(grid)
                    .children(nav),
            })
            .when(emoji_tab, |d| {
                d.when_some(status, |d, error| {
                    // `.emojiStatus`.
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .p(px(8.))
                            .rounded(px(8.))
                            .bg(c.surface_panel)
                            .shadow(buzz_ui::tokens::shadow::xs())
                            .text_caption()
                            .child(error)
                            .child(
                                Button::new("emoji-retry", "Retry emoji")
                                    .on_click(move |_, _, _| session.refresh_emoji()),
                            ),
                    )
                })
            })
            .when(self.tones_open && emoji_tab, |d| {
                d.child(div().absolute().inset_0().occlude().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.tones_open = false;
                        this.preview_tone = None;
                        cx.notify();
                    }),
                ))
                .child(self.tone_menu(cx))
            })
    }
}

impl Render for EmojiPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.plugin_picker(window, cx) {
            Some(view) => {
                let c = *cx.colors();
                div()
                    .w(px(mart::width(9, f32::from(window.viewport_size().width))))
                    .max_h(px(MART_HEIGHT))
                    .floating_surface(&c)
                    .overflow_hidden()
                    .occlude()
                    .p(px(12.))
                    .child(view)
                    .into_any_element()
            }
            None => self.render_grid(window, cx).into_any_element(),
        }
    }
}

/// Picks for a plugin's reaction picker: the emoji it chose.
pub type Select = Rc<dyn Fn(String, &mut Window, &mut App)>;
/// Makes a plugin's reaction picker view, for reacting in `community`.
pub type BuildReactionPicker =
    Rc<dyn Fn(Select, &Entity<Community>, &mut Window, &mut App) -> AnyView>;

/// A reaction picker a plugin adds (a composer tool's
/// `reactionComponent`). Of these and the bundled emoji picker
/// (`buzz.emoji/picker` at 0), the lowest order, then the lowest key,
/// picks reactions (`selectReactionTool`).
#[derive(Clone)]
pub struct ReactionPicker {
    pub key: &'static str,
    pub order: i32,
    pub build: BuildReactionPicker,
}

#[derive(Default)]
struct ReactionPickers(Vec<ReactionPicker>);

impl Global for ReactionPickers {}

pub fn register_reaction_picker(picker: ReactionPicker, cx: &mut App) {
    let pickers = &mut cx.default_global::<ReactionPickers>().0;
    pickers.retain(|existing| existing.key != picker.key);
    pickers.push(picker);
}

pub fn unregister_reaction_picker(key: &str, cx: &mut App) {
    cx.default_global::<ReactionPickers>()
        .0
        .retain(|existing| existing.key != key);
}

/// Whether tool `key` also picks reactions (`reactionComponent`).
pub fn picks_reactions(key: &str, cx: &App) -> bool {
    cx.try_global::<ReactionPickers>()
        .is_some_and(|pickers| pickers.0.iter().any(|picker| picker.key == key))
}

/// Whether any reaction picker is on: Emoji's own or a plugin's. With
/// neither, messages offer no "Add reaction".
pub fn reactions_available(cx: &App) -> bool {
    crate::plugins::enabled("buzz.emoji") || winning_picker(cx).is_some()
}

/// The plugin picker that outranks the bundled one, if any.
fn winning_picker(cx: &App) -> Option<ReactionPicker> {
    const BUNDLED: (i32, &str) = (0, "buzz.emoji/picker");
    cx.try_global::<ReactionPickers>()?
        .0
        .iter()
        .filter(|picker| crate::plugins::enabled_key(picker.key))
        // The bundled picker counts only while Emoji is on.
        .filter(|picker| {
            !crate::plugins::enabled_key(BUNDLED.1) || (picker.order, picker.key) < BUNDLED
        })
        .min_by_key(|picker| (picker.order, picker.key))
        .cloned()
}

impl EmojiPicker {
    /// A picker for reactions, which a plugin's reaction picker can stand
    /// in for.
    pub fn for_reactions(mut self) -> Self {
        self.reactions = true;
        self
    }

    /// The winning plugin reaction picker's view, made once per picker.
    fn plugin_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyView> {
        if !self.reactions {
            return None;
        }
        let Some(winner) = winning_picker(cx) else {
            self.plugin_view = None;
            return None;
        };
        if let Some((key, view)) = &self.plugin_view
            && *key == winner.key
        {
            return Some(view.clone());
        }
        let picker = cx.entity().downgrade();
        // A `:shortcode:` the community's catalog has is that custom
        // emoji (`catalogEmoji`), so it toggles the same reaction.
        let select: Select = Rc::new(move |emoji, _, cx| {
            let _ = picker.update(cx, |picker, cx| {
                let custom = emoji
                    .strip_prefix(':')
                    .and_then(|rest| rest.strip_suffix(':'))
                    .map(str::to_lowercase)
                    .and_then(|code| {
                        picker
                            .community
                            .read(cx)
                            .snapshot
                            .emoji
                            .entries
                            .iter()
                            .find(|entry| entry.shortcode == code)
                            .cloned()
                    });
                match custom {
                    Some(custom) => cx.emit(PickedAs(emoji, custom)),
                    None => cx.emit(Picked(Pick::Unicode(emoji))),
                }
            });
        });
        let community = self.community.clone();
        let view = (winner.build)(select, &community, window, cx);
        self.plugin_view = Some((winner.key, view.clone()));
        Some(view)
    }
}

impl EmojiPicker {
    /// Mart's skin tone button: in the category bar, or beside the search
    /// field when the bar is hidden.
    fn tone_button(&self, in_nav: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let skin = mart_store(cx).skin;
        let open = self.tones_open;
        let button = div()
            .id("emoji-tone-open")
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .when(open, |d| d.bg(c.affordance_subtle_hover))
            .tooltip(Tooltip::text(CHOOSE_TONE))
            .child(swatch(skin))
            .on_click(cx.listener(|this, _, _, cx| {
                this.tones_open = true;
                cx.notify();
            }));
        if in_nav {
            div()
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    button
                        .size(px(28.))
                        .hover(|s| s.bg(c.affordance_subtle_hover)),
                )
                .into_any_element()
        } else {
            div()
                .size(px(CELL))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    button
                        .size(px(EMOJI_SIZE))
                        .border_1()
                        .border_color(transparent_black())
                        .when(open, |d| d.bg(c.border_standard))
                        .hover(|s| s.border_color(c.border_standard)),
                )
                .into_any_element()
        }
    }

    /// Mart's skin tone menu, above the bar at the right.
    fn tone_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let skin = mart_store(cx).skin;
        div()
            .absolute()
            .right(px(8.))
            .bottom(px(NAV))
            .flex()
            .flex_col()
            .p(px(4.))
            .bg(c.surface_popover)
            .border_1()
            .border_color(c.border_standard)
            .rounded(px(8.))
            .shadow(buzz_ui::tokens::shadow::sm())
            .occlude()
            .children((1..=TONES.len()).map(|option| {
                let checked = option == skin;
                div()
                    .id(("emoji-skin", option))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(6.))
                    .py(px(4.))
                    .pr(px(10.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_body_sm()
                    .hover(|s| s.bg(c.affordance_subtle_hover))
                    .when(checked, |d| {
                        // A ring, not a fill: the shadow sits behind the option.
                        d.bg(c.surface_popover).shadow(vec![
                            BoxShadow::new(px(0.), px(0.), c.text_standard).spread_radius(px(2.)),
                        ])
                    })
                    .child(swatch(option))
                    .child(TONES[option - 1].1)
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.preview_tone = Some(option);
                        } else if this.preview_tone == Some(option) {
                            this.preview_tone = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        set_skin(option, cx);
                        this.tones_open = false;
                        this.preview_tone = None;
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }
}

/// The picker as a popup: it takes the pointer and calls `on_dismiss`
/// for a press anywhere else. Callers anchor it.
pub fn emoji_picker(
    picker: &Entity<EmojiPicker>,
    on_dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .occlude()
        .on_mouse_down_out(on_dismiss)
        .child(picker.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn codes(found: &[Candidate]) -> Vec<&str> {
        found.iter().map(Candidate::shortcode).collect()
    }

    #[test]
    fn ranks_prefix_shortcodes_first_like_the_reference() {
        let found = search("smi", &[], 7);
        assert_eq!(
            codes(&found),
            [
                "smile",
                "smirk",
                "smiley",
                "smile_cat",
                "smirk_cat",
                "smiley_cat",
                "smiling_imp"
            ]
        );
        assert_eq!(codes(&search("+1", &[], 1)), ["+1"]);
        assert!(search("", &[], 5).is_empty());
    }

    #[test]
    fn finds_custom_emoji_by_shortcode() {
        let custom = [CustomEmoji {
            shortcode: "partyparrot".into(),
            url: "https://x.test/p.gif".into(),
        }];
        let found = search("partyp", &custom, 3);
        assert_eq!(found[0], Candidate::Custom(custom[0].clone()));
        assert_eq!(found[0].pick(None).text(), ":partyparrot:");
    }

    #[test]
    fn applies_skin_tones_where_they_exist() {
        let wave = Candidate::Unicode(emojis::get("👋").unwrap());
        assert_eq!(wave.pick(Some(SkinTone::Dark)).text(), "👋🏿");
        let grin = Candidate::Unicode(emojis::get("😀").unwrap());
        assert_eq!(grin.pick(Some(SkinTone::Dark)).text(), "😀");
    }

    #[test]
    fn keeps_only_valid_saved_reactions() {
        let long = "x".repeat(65);
        let json = format!(
            r#"[{{"emoji":"🎉","count":2,"used":5}},
               {{"emoji":"👍","count":4,"used":1}},
               {{"emoji":" ","count":1,"used":1}},
               {{"emoji":"{long}","count":1,"used":1}},
               {{"emoji":"🙂","count":0,"used":1}},
               {{"emoji":"🙃","count":1.5,"used":1}},
               {{"emoji":"😐","count":1,"used":-1}},
               {{"emoji":7,"count":1,"used":1}},
               {{"emoji":":party_parrot:","count":1,"used":9}}]"#
        );
        let kept: Vec<String> = read_usage(&json).into_iter().map(|u| u.emoji).collect();
        assert_eq!(kept, ["👍", "🎉", ":party_parrot:"]);
        assert!(read_usage("{\"emoji\":\"👍\"}").is_empty());
        assert!(read_usage("not json").is_empty());
    }

    #[test]
    fn maps_frequent_ids_back_to_emoji() {
        let custom = [CustomEmoji {
            shortcode: "bufo".into(),
            url: "https://x.test/b.png".into(),
        }];
        let unicode = |id| match frequent_candidate(id, "here", &custom) {
            Some(Candidate::Unicode(emoji)) => Some(emoji.as_str()),
            _ => None,
        };
        assert_eq!(unicode("+1"), Some("👍"));
        // Not one of Mart's ids: kept by its text.
        assert_eq!(unicode("😀"), Some("😀"));
        assert_eq!(
            frequent_candidate(&mart::custom_id("here", "bufo"), "here", &custom),
            Some(Candidate::Custom(custom[0].clone()))
        );
        assert_eq!(
            frequent_candidate(&mart::custom_id("there", "bufo"), "here", &custom),
            None
        );
    }
}
