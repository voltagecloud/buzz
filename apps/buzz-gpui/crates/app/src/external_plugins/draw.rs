//! Draws a plugin's element tree with the app's components, for its
//! views and for what it renders inside messages. Clicks and toggles go
//! to `send`; fields come ready-made from the caller, which keeps their
//! text.

use super::protocol::{
    Align, AvatarSize, Background, ButtonStyle, CellKind, CodeLine, CodeTone, Gap, Justify, Node,
    TextStyle, Tone,
};
use crate::community::Community;
use crate::ui::markdown;
use crate::ui::navigation;
use crate::ui::overlay;
use buzz_ui::{
    ActiveTheme, Avatar, Badge, BadgeTone, Button, ButtonSize, ButtonVariant, Checkbox,
    CheckboxState, Colors, Divider, Icon, IconButton, IconButtonSize, IconName, Menu, MenuItem,
    MenuLabel, PanelHeader, Popover, Radio, Select, Spinner, Switch, TextField, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::rc::Rc;

/// Sends an element's event to the plugin: its id, the event and a value.
pub type Send = Rc<dyn Fn(&str, &str, Option<Value>, &mut App)>;

pub struct Draw<'a> {
    pub send: Send,
    /// The text fields by id; an input without one draws nothing.
    pub fields: &'a HashMap<String, FieldState>,
    /// Markdown resolves mentions and channels in this community.
    pub community: Option<Entity<Community>>,
    /// Names where Markdown shows, and keeps element ids apart.
    pub place: &'a str,
    /// Buzz's own views the tree embeds, by element id; an embed without
    /// one draws nothing.
    pub embeds: &'a HashMap<String, AnyView>,
    /// For menus and popovers, which open over the window; without one
    /// (inside a message) they do not open.
    pub window: Option<&'a Window>,
}

/// A composer or thread in the tree, for the caller to make its view.
pub enum EmbedNode<'a> {
    Composer {
        id: &'a str,
        channel: &'a str,
        thread: Option<&'a str>,
        parent: Option<&'a str>,
        placeholder: Option<&'a str>,
        /// The plugin takes the draft (`submit: "plugin"`).
        plugin_submits: bool,
        disabled: bool,
        auto_focus: bool,
        draft: Option<&'a str>,
        mentions: &'a [Value],
        invite_agents: bool,
        draft_key: Option<&'a str>,
    },
    Thread {
        id: &'a str,
        channel: &'a str,
        root: &'a str,
    },
    Message {
        id: &'a str,
        channel: &'a str,
        message: &'a str,
        style: super::message_view::Style,
    },
}

/// Every embedded composer and thread in `node`.
pub fn embeds(node: &Node) -> Vec<EmbedNode<'_>> {
    let mut found = Vec::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node {
            container if !container.children().is_empty() => {
                stack.extend(container.children().into_iter().rev())
            }
            Node::Composer {
                id,
                channel,
                thread,
                parent,
                placeholder,
                submit,
                disabled,
                auto_focus,
                draft,
                mentions,
                invite_agents,
                draft_key,
            } => found.push(EmbedNode::Composer {
                id,
                channel,
                thread: thread.as_deref(),
                parent: parent.as_deref(),
                placeholder: placeholder.as_deref(),
                plugin_submits: submit.as_deref() == Some("plugin"),
                disabled: *disabled,
                auto_focus: *auto_focus,
                draft: draft.as_deref(),
                mentions,
                invite_agents: *invite_agents,
                draft_key: draft_key.as_deref(),
            }),
            Node::Thread {
                id, channel, root, ..
            } => found.push(EmbedNode::Thread { id, channel, root }),
            Node::Message {
                id,
                channel,
                message,
                layout,
                compact,
                actions,
            } => found.push(EmbedNode::Message {
                id,
                channel,
                message,
                style: super::message_view::Style {
                    layout: match layout.as_deref() {
                        Some("thread") => crate::ui::message::Layout::Thread,
                        Some("nested") => crate::ui::message::Layout::Nested,
                        _ => crate::ui::message::Layout::Timeline,
                    },
                    compact: *compact,
                    actions: actions.unwrap_or(true),
                },
            }),
            _ => {}
        }
    }
    found
}

/// A text field's state: one line, or a text area.
#[derive(Clone)]
pub enum FieldState {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
}

impl FieldState {
    pub fn set_text(&self, text: String, window: &mut Window, cx: &mut App) {
        match self {
            FieldState::Line(state) => {
                state.update(cx, |state, cx| state.set_value(text, window, cx))
            }
            FieldState::Area(state) => {
                state.update(cx, |state, cx| state.set_value(text, window, cx))
            }
        }
    }
}

/// An input in the tree, for the caller to make its field.
pub struct InputNode<'a> {
    pub id: &'a str,
    pub value: Option<&'a String>,
    pub placeholder: Option<&'a String>,
    pub version: u64,
    pub clear_on_submit: bool,
    /// A text area's least and most lines; `None` for one line.
    pub area: Option<(usize, usize)>,
}

/// Every input in `node`.
pub fn inputs(node: &Node) -> Vec<InputNode<'_>> {
    let mut found = Vec::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node {
            container if !container.children().is_empty() => {
                stack.extend(container.children().into_iter().rev())
            }
            Node::Input {
                id,
                value,
                placeholder,
                version,
                clear_on_submit,
                ..
            } => found.push(InputNode {
                id,
                value: value.as_ref(),
                placeholder: placeholder.as_ref(),
                version: *version,
                clear_on_submit: *clear_on_submit,
                area: None,
            }),
            Node::Textarea {
                id,
                value,
                placeholder,
                version,
                rows,
                max_rows,
                ..
            } => {
                let least = rows.unwrap_or(3).clamp(1, 40) as usize;
                found.push(InputNode {
                    id,
                    value: value.as_ref(),
                    placeholder: placeholder.as_ref(),
                    version: *version,
                    clear_on_submit: false,
                    area: Some((
                        least,
                        (max_rows.unwrap_or(8).clamp(1, 40) as usize).max(least),
                    )),
                })
            }
            _ => {}
        }
    }
    found
}

fn gap_of(div: Div, gap: Gap) -> Div {
    match gap {
        Gap::None => div,
        Gap::Sm => div.gap(space::S2),
        Gap::Md => div.gap(space::S4),
        Gap::Lg => div.gap(space::S6),
    }
}

fn padding_of(div: Div, padding: Gap) -> Div {
    match padding {
        Gap::None => div,
        Gap::Sm => div.p(space::S2),
        Gap::Md => div.p(space::S4),
        Gap::Lg => div.p(space::S6),
    }
}

fn background_of(div: Div, background: Option<Background>, c: &Colors) -> Div {
    match background {
        Some(Background::Inset) => div.bg(c.surface_inset),
        Some(Background::Panel) => div.bg(c.surface_panel),
        None => div,
    }
}

/// One `ch` of the 12px mono face (JetBrains Mono advances 0.6em).
const CH: f32 = 0.75 * 0.6;
/// A line number's gutter: 7ch.
const GUTTER: f32 = 7. * CH;

/// A `code` block's lines: gutters of line numbers, then the text,
/// filled by tone, as a diff draws them.
fn code(lines: &[CodeLine], wrap: bool, c: &Colors) -> Div {
    let tone = |tone: CodeTone| match tone {
        CodeTone::Added => (Some(c.affordance_success), c.text_success),
        CodeTone::Removed => (Some(c.affordance_danger), c.text_danger),
        CodeTone::Hunk | CodeTone::Meta => (Some(c.surface_inset), c.text_subtle),
        CodeTone::Context => (None, c.text_standard),
    };
    let cell = |text: &str, cell_tone: CodeTone, kind: CellKind| {
        let (fill, color) = tone(cell_tone);
        let cell = match kind {
            CellKind::Number => div()
                .flex_none()
                .w(rems(GUTTER))
                .px(rems(CH))
                .flex()
                .justify_end()
                .whitespace_nowrap()
                .child(text.to_owned()),
            // An empty side, marked by a 2px rule at 4.6ch.
            CellKind::Omitted => {
                return div().flex_none().w(rems(GUTTER)).relative().child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(rems(5.6 * CH))
                        .w(px(2.))
                        .bg(c.border_standard),
                );
            }
            CellKind::Text => {
                let cell = div()
                    .flex_1()
                    .min_w_0()
                    .pl(rems(0.375))
                    .child(text.to_owned());
                if wrap { cell } else { cell.whitespace_nowrap() }
            }
        };
        let cell = cell.text_color(color);
        match fill {
            Some(fill) => cell.bg(fill),
            None => cell,
        }
    };
    div()
        .flex()
        .flex_col()
        .w_full()
        .text_mono()
        .children(lines.iter().map(|line| {
            let row = div().flex().w_full().min_h(rems(1.));
            if line.cells.is_empty() {
                let text = line.text.as_deref().unwrap_or_default();
                row.child(cell(text, line.tone, CellKind::Text))
            } else {
                row.children(
                    line.cells
                        .iter()
                        .map(|one| cell(&one.text, one.tone, one.kind)),
                )
            }
        }))
}

thread_local! {
    /// The menu or popover open now, by element id: one at a time.
    static OPEN: std::cell::RefCell<Option<SharedString>> = const { std::cell::RefCell::new(None) };
}

fn is_open(id: &SharedString) -> bool {
    OPEN.with_borrow(|open| open.as_ref() == Some(id))
}

fn set_open(id: Option<SharedString>, cx: &mut App) {
    OPEN.with_borrow_mut(|open| *open = id);
    cx.refresh_windows();
}

/// The community's media, for drawing pictures from the web.
fn media_of(draw: &Draw, cx: &App) -> Option<buzz_relay_client::Media> {
    draw.community
        .clone()
        .or_else(|| crate::community::AppState::current(cx))
        .map(|community| community.read(cx).session.media().clone())
}

thread_local! {
    /// Prepared documents by their text, so a redraw does not parse again.
    static DOCUMENTS: std::cell::RefCell<HashMap<u64, Rc<crate::ui::document::body::Body>>> =
        Default::default();
    /// Measured widths of collapsibles and thumbnail rows, by element id.
    static WIDTHS: std::cell::RefCell<HashMap<SharedString, f32>> = Default::default();
}

fn document(text: &str, html: Option<&str>, base: &str) -> Rc<crate::ui::document::body::Body> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    (text, html, base).hash(&mut hasher);
    let key = hasher.finish();
    DOCUMENTS.with_borrow_mut(|documents| {
        if documents.len() > 512 && !documents.contains_key(&key) {
            documents.clear();
        }
        documents
            .entry(key)
            .or_insert_with(|| Rc::new(crate::ui::document::body::prepare(text, html, base)))
            .clone()
    })
}

fn width_of(key: &SharedString) -> f32 {
    WIDTHS.with_borrow(|widths| widths.get(key).copied().unwrap_or(0.))
}

/// Keeps `key`'s laid-out width, drawing again when it changed.
fn remember_width(key: &SharedString, width: Pixels, cx: &mut App) {
    let width = f32::from(width);
    let changed = WIDTHS.with_borrow_mut(|widths| {
        if widths.len() > 2048 && !widths.contains_key(key) {
            widths.clear();
        }
        let old = widths.insert(key.clone(), width);
        old.is_none_or(|old| (old - width).abs() > 0.5)
    });
    if changed {
        cx.refresh_windows();
    }
}

/// Whether `text` fits `available` pixels on one line in the small body
/// face.
fn fits_one_line(text: &str, available: f32, window: &Window) -> bool {
    if available <= 0. || text.contains('\n') {
        return false;
    }
    let size = rems(0.875).to_pixels(window.rem_size());
    let mut font = gpui_kit::font(buzz_ui::tokens::font::SANS);
    font.fallbacks = Some(buzz_ui::fonts::fallbacks());
    let run = TextRun {
        len: text.len(),
        font,
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(text.to_string().into(), size, &[run], None);
    f32::from(line.width) <= available
}

/// Someone's picture, or their initial, from the community's profiles.
fn avatar_of(pubkey: &str, size: buzz_ui::AvatarSize, draw: &Draw, cx: &App) -> Avatar {
    let profile = draw.community.as_ref().and_then(|community| {
        community
            .read(cx)
            .snapshot
            .profiles
            .get(&pubkey.to_ascii_lowercase())
            .cloned()
    });
    let name = profile
        .as_ref()
        .map(|profile| profile.name.clone())
        .unwrap_or_else(|| pubkey.chars().take(2).collect());
    let mut avatar = Avatar::new(name).size(size);
    if let Some(picture) = profile.and_then(|profile| profile.picture.clone())
        && picture.starts_with("https://")
    {
        avatar = avatar.image(crate::ui::media::any_avatar(&picture));
    }
    avatar
}

/// An icon in a tone, for menus and their triggers.
fn toned_icon(name: &str, tone: Tone, c: &Colors) -> Icon {
    let icon = Icon::new(super::icon(Some(name))).size(16.);
    match tone_color(tone, c) {
        Some(color) => icon.color(color),
        None => icon,
    }
}

thread_local! {
    /// Scroll areas' handles by element id, so a wheel they cannot use
    /// passes to the page.
    static SCROLLS: std::cell::RefCell<HashMap<SharedString, ScrollHandle>> = Default::default();
}

fn scroll_handle(id: &SharedString) -> ScrollHandle {
    SCROLLS.with_borrow_mut(|handles| {
        if handles.len() > 1024 && !handles.contains_key(id) {
            handles.clear();
        }
        handles.entry(id.clone()).or_default().clone()
    })
}

/// Lets the page scroll once the area cannot, as a browser chains wheel
/// scrolling. Runs after the area applied the delta.
fn chain_scroll(handle: ScrollHandle) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) {
    move |event, window, cx| {
        let delta = event.delta.pixel_delta(window.line_height()).y;
        let max = handle.max_offset().y;
        let before = -handle.offset().y + delta;
        let consumed = (delta < px(0.) && before < max) || (delta > px(0.) && before > px(0.));
        if consumed {
            cx.stop_propagation();
        }
    }
}

fn justify_of(div: Div, justify: Justify) -> Div {
    match justify {
        Justify::Start => div,
        Justify::Center => div.justify_center(),
        Justify::End => div.justify_end(),
        Justify::Between => div.justify_between(),
    }
}

fn tone_color(tone: Tone, c: &Colors) -> Option<Hsla> {
    match tone {
        Tone::Default => None,
        Tone::Subtle => Some(c.text_subtle),
        Tone::Accent => Some(c.text_accent),
        Tone::Success => Some(c.text_success),
        Tone::Warning => Some(c.text_warning),
        Tone::Danger => Some(c.text_danger),
        Tone::Complete => Some(c.icon_status_complete),
        Tone::Progress => Some(c.icon_status_progress),
    }
}

fn children(nodes: &[Node], path: &str, draw: &Draw, cx: &App) -> Vec<AnyElement> {
    nodes
        .iter()
        .enumerate()
        .map(|(index, child)| node(child, &format!("{path}-{index}"), draw, cx))
        .collect()
}

/// Draws `node`; `path` names its place in the tree.
pub fn node(node: &Node, path: &str, draw: &Draw, cx: &App) -> AnyElement {
    let c = *cx.colors();
    let element_id =
        |kind: &str| SharedString::from(format!("plugin-{}-{kind}-{path}", draw.place));
    match node {
        Node::Column {
            children: nodes,
            gap,
            align,
            justify,
            grow,
            padding,
            background,
        } => {
            let column = gap_of(div().flex().flex_col().min_w_0(), *gap);
            let column = background_of(column, *background, &c);
            let column = match align {
                Align::Start => column.items_start(),
                Align::Center => column.items_center(),
                Align::End => column.items_end(),
                Align::Stretch => column,
            };
            let column = padding_of(justify_of(column, *justify), *padding);
            let column = if *grow { column.flex_1() } else { column };
            column
                .children(children(nodes, path, draw, cx))
                .into_any_element()
        }
        Node::Row {
            children: nodes,
            gap,
            justify,
            wrap,
            grow,
            padding,
            background,
        } => {
            let row = gap_of(div().flex().flex_row().items_center().min_w_0(), *gap);
            let row = padding_of(background_of(row, *background, &c), *padding);
            let row = justify_of(row, *justify);
            let row = if *wrap { row.flex_wrap() } else { row };
            let row = if *grow { row.flex_1() } else { row };
            row.children(children(nodes, path, draw, cx))
                .into_any_element()
        }
        Node::Header {
            title,
            icon,
            subtitle,
            children: nodes,
        } => {
            let mut header = PanelHeader::new().joined(true).label(
                icon.as_deref().map(|name| super::icon(Some(name))),
                title.clone(),
            );
            if let Some(subtitle) = subtitle {
                header = header.leading(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child(subtitle.clone()),
                );
            }
            for action in children(nodes, path, draw, cx) {
                header = header.action(action);
            }
            header.into_any_element()
        }
        Node::Card {
            title,
            children: nodes,
            padding,
            gap,
        } => padding_of(gap_of(div().flex().flex_col().min_w_0(), *gap), *padding)
            .overflow_hidden()
            .border_1()
            .border_color(c.border_standard)
            .rounded(rems(0.75))
            .children(
                title
                    .as_ref()
                    .map(|title| div().text_label().child(title.clone())),
            )
            .children(children(nodes, path, draw, cx))
            .into_any_element(),
        Node::Text {
            text,
            style,
            tone,
            truncate,
            tooltip,
        } => {
            let text = div().child(text.clone());
            let text = if *truncate {
                text.min_w_0().truncate()
            } else {
                text
            };
            let text = match style {
                TextStyle::Body => text.text_body(),
                TextStyle::Small => text.text_body_sm(),
                TextStyle::Large => text.text_body_lg(),
                TextStyle::Label => text.text_label(),
                TextStyle::Title => text.text_title(),
                TextStyle::Heading => text.text_heading(),
                TextStyle::Code => text.text_mono(),
            };
            let text = match tone_color(*tone, &c) {
                Some(color) => text.text_color(color),
                None => text,
            };
            match tooltip {
                Some(tooltip) => text
                    .id(element_id("text"))
                    .tooltip(buzz_ui::Tooltip::text(tooltip.clone()))
                    .into_any_element(),
                None => text.into_any_element(),
            }
        }
        Node::Markdown { text } => match &draw.community {
            Some(community) => markdown::text(text, draw.place, community.read(cx), cx),
            None => div().text_body().child(text.clone()).into_any_element(),
        },
        Node::Button {
            id,
            label,
            variant,
            icon,
            disabled,
            loading,
            size,
            tooltip,
            fill,
        } => {
            let small = size.as_deref() == Some("sm");
            if label.is_empty()
                && let Some(icon) = icon.as_deref()
            {
                let send = draw.send.clone();
                let id = id.clone();
                let mut button = IconButton::new(element_id("button"), super::icon(Some(icon)))
                    .size(if small {
                        IconButtonSize::Sm
                    } else {
                        IconButtonSize::Md
                    })
                    .variant(match variant {
                        ButtonStyle::Ghost => ButtonVariant::Ghost,
                        _ => ButtonVariant::Subtle,
                    })
                    .disabled(*disabled || *loading);
                if *loading {
                    button = button.child(Spinner::new().size(16.));
                }
                if let Some(tooltip) = tooltip {
                    button = button.tooltip(tooltip.clone());
                }
                return button
                    .on_click(move |_, _, cx| send(&id, "click", None, cx))
                    .into_any_element();
            }
            let (variant, destructive) = match variant {
                ButtonStyle::Prominent => (ButtonVariant::Prominent, false),
                ButtonStyle::Subtle => (ButtonVariant::Subtle, false),
                ButtonStyle::Ghost => (ButtonVariant::Ghost, false),
                ButtonStyle::Outline => (ButtonVariant::Outline, false),
                ButtonStyle::Destructive => (ButtonVariant::Subtle, true),
            };
            let mut button = Button::new(element_id("button"), label.clone())
                .size(if small {
                    ButtonSize::Sm
                } else {
                    ButtonSize::Md
                })
                .variant(variant)
                .disabled(*disabled)
                .loading(*loading);
            if destructive {
                button = button.destructive();
            }
            if let Some(tooltip) = tooltip {
                button = button.tooltip(tooltip.clone());
            }
            if let Some(icon) = icon.as_deref().map(|name| super::icon(Some(name))) {
                button = button.icon(icon);
            }
            if *fill {
                button = button.full_width();
            }
            let send = draw.send.clone();
            let id = id.clone();
            // Sized to its label, even in a column, unless it fills it.
            let place = div().flex();
            let place = if *fill { place.w_full() } else { place };
            place
                .child(button.on_click(move |_, _, cx| send(&id, "click", None, cx)))
                .into_any_element()
        }
        Node::Link {
            label,
            url,
            external,
        } => {
            let url = url.clone();
            let external = *external;
            div()
                .id(element_id("link"))
                .text_body()
                .text_color(c.text_accent)
                .cursor_pointer()
                .hover(|style| style.underline())
                .child(label.clone())
                .on_click(move |event, window, cx| {
                    if url.starts_with("buzz://") {
                        navigation::open_url(&url, window, cx);
                    } else if external
                        && (url.starts_with("https://") || url.starts_with("http://"))
                    {
                        cx.open_url(&url);
                    } else if url.starts_with("https://") || url.starts_with("http://") {
                        navigation::open_web_link(&url, event.modifiers(), window, cx);
                    }
                })
                .into_any_element()
        }
        Node::Input {
            id, disabled, grow, ..
        } => match draw.fields.get(id) {
            Some(FieldState::Line(state)) => {
                let field = TextField::new(state).disabled(*disabled);
                if *grow {
                    div().flex_1().min_w_0().child(field).into_any_element()
                } else {
                    field.into_any_element()
                }
            }
            _ => div().into_any_element(),
        },
        Node::Textarea { id, disabled, .. } => match draw.fields.get(id) {
            Some(FieldState::Area(state)) => buzz_ui::TextArea::new(state)
                .disabled(*disabled)
                .into_any_element(),
            _ => div().into_any_element(),
        },
        Node::Checkbox {
            id,
            checked,
            label,
            indeterminate,
            completed,
            disabled,
        } => {
            let send = draw.send.clone();
            let id = id.clone();
            let state = match (*checked, *indeterminate) {
                (true, _) => CheckboxState::Checked,
                (false, true) => CheckboxState::Indeterminate,
                (false, false) => CheckboxState::Unchecked,
            };
            let mut checkbox = Checkbox::new(element_id("checkbox"), state).disabled(*disabled);
            if let Some(label) = label {
                checkbox = if *completed {
                    checkbox.label_element(
                        div()
                            .min_w_0()
                            .text_color(c.text_subtle)
                            .line_through()
                            .child(label.clone()),
                    )
                } else {
                    checkbox.label(label.clone())
                };
            }
            checkbox
                .on_change(move |on, _, cx| send(&id, "toggle", Some(json!(on)), cx))
                .into_any_element()
        }
        Node::Choice { id, value, options } => div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .children(options.iter().take(50).enumerate().map(|(index, option)| {
                let send = draw.send.clone();
                let (id, chosen) = (id.clone(), option.value.clone());
                Radio::new(
                    element_id(&format!("choice-{index}")),
                    option.label.clone(),
                    value.as_ref() == Some(&option.value),
                )
                .on_select(move |_, cx| send(&id, "change", Some(json!(chosen)), cx))
                .into_any_element()
            }))
            .into_any_element(),
        Node::Progress { value } => div()
            .w_full()
            .h(px(6.))
            .rounded(px(3.))
            .bg(c.border_standard)
            .child(
                div()
                    .h_full()
                    .rounded(px(3.))
                    .bg(c.text_accent)
                    .w(relative(value.clamp(0., 1.))),
            )
            .into_any_element(),
        Node::Audio {
            url,
            name,
            mime,
            duration,
            waveform,
            voice,
        } => {
            use buzz_relay_client::fold::{Attachment, AttachmentKind};
            let Some(community) = &draw.community else {
                return div().into_any_element();
            };
            let media = community.read(cx).session.media().clone();
            let attachment = Attachment {
                url: url.clone(),
                kind: AttachmentKind::Audio,
                mime: mime.clone(),
                size: None,
                name: name.clone(),
                duration: *duration,
                waveform: waveform.clone(),
                voice_note: *voice,
                dimensions: None,
                blurhash: None,
                preview_url: None,
                insecure: false,
            };
            let id = element_id("audio");
            let player = if *voice {
                crate::ui::media::voice_note(id, &attachment, &media, cx)
            } else {
                crate::ui::media::audio_attachment(id, &attachment, &media, cx)
            };
            // The card's own width (21rem), even inside a row.
            div()
                .flex_none()
                .w(px(336.))
                .max_w_full()
                .child(player)
                .into_any_element()
        }
        Node::Image {
            url,
            width,
            height,
            alt,
        } => {
            let safe = url.starts_with("https://")
                || (url.starts_with("http://") && url.contains("://localhost"));
            let source = match &draw.community {
                Some(community) if safe => Some(crate::ui::media::image(
                    url,
                    &community.read(cx).session.media().clone(),
                )),
                _ if safe => Some(ImageSource::from(url.clone())),
                _ => None,
            };
            let width = width.map(|width| width.min(1200) as f32);
            let height = height.map(|height| height.min(1200) as f32);
            match source {
                Some(source) => {
                    let mut image = img(source).object_fit(ObjectFit::Contain).max_w_full();
                    if let Some(width) = width {
                        image = image.w(px(width));
                    }
                    if let Some(height) = height {
                        image = image.h(px(height));
                    }
                    image.into_any_element()
                }
                None => div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(alt.clone().unwrap_or_else(|| "Image".to_owned()))
                    .into_any_element(),
            }
        }
        Node::Message { id, .. } => match draw.embeds.get(id) {
            Some(view) => div().w_full().child(view.clone()).into_any_element(),
            None => div().into_any_element(),
        },
        Node::Composer { id, .. } => match draw.embeds.get(id) {
            Some(view) => div().w_full().child(view.clone()).into_any_element(),
            None => div().into_any_element(),
        },
        Node::Thread { id, height, .. } => match draw.embeds.get(id) {
            Some(view) => div()
                .w_full()
                .h(px(height.unwrap_or(480).clamp(120, 1200) as f32))
                .border_1()
                .border_color(c.border_standard)
                .rounded(rems(0.75))
                .overflow_hidden()
                .child(view.clone())
                .into_any_element(),
            None => div().into_any_element(),
        },
        Node::Avatar {
            pubkey,
            url,
            name,
            size,
        } => {
            let size = match size {
                AvatarSize::Small => buzz_ui::AvatarSize::Small,
                AvatarSize::Default => buzz_ui::AvatarSize::Default,
                AvatarSize::Large => buzz_ui::AvatarSize::Large,
            };
            match url {
                // A picture from the web, such as a GitHub user's.
                Some(url) if url.starts_with("https://") => {
                    let mut avatar =
                        Avatar::new(name.clone().unwrap_or_else(|| "?".to_owned())).size(size);
                    if let Some(media) = media_of(draw, cx) {
                        avatar = avatar.image(crate::ui::media::image(url, &media));
                    }
                    avatar.into_any_element()
                }
                // A name without a picture shows its initial.
                _ if pubkey.is_empty() => {
                    Avatar::new(name.clone().unwrap_or_else(|| "?".to_owned()))
                        .size(size)
                        .into_any_element()
                }
                _ => avatar_of(pubkey, size, draw, cx).into_any_element(),
            }
        }
        Node::Switch { id, checked, label } => {
            let send = draw.send.clone();
            let id = id.clone();
            let mut switch = Switch::new(element_id("switch"), *checked);
            if let Some(label) = label {
                switch = switch.label(label.clone());
            }
            switch
                .on_change(move |on, _, cx| send(&id, "toggle", Some(json!(on)), cx))
                .into_any_element()
        }
        Node::Badge { text, tone } => Badge::new(text.clone())
            .tone(match tone {
                Tone::Default | Tone::Subtle => BadgeTone::Neutral,
                Tone::Accent => BadgeTone::Accent,
                Tone::Success => BadgeTone::Success,
                Tone::Warning => BadgeTone::Warning,
                Tone::Danger => BadgeTone::Danger,
                Tone::Complete => BadgeTone::Success,
                Tone::Progress => BadgeTone::Accent,
            })
            .into_any_element(),
        Node::Icon { name, size, tone } => {
            let icon =
                Icon::new(super::icon(Some(name))).size(size.unwrap_or(16).clamp(8, 128) as f32);
            match tone_color(*tone, &c) {
                Some(color) => icon.color(color),
                None => icon,
            }
            .into_any_element()
        }
        Node::Menu {
            id,
            label,
            icon,
            icon_tone,
            avatar,
            tooltip,
            title,
            value,
            options,
            disabled,
        } => {
            let key = element_id("menu");
            let mut trigger = IconButton::new(key.clone(), IconName::User)
                .size(IconButtonSize::Sm)
                .variant(if avatar.is_some() {
                    ButtonVariant::Avatar
                } else {
                    ButtonVariant::Ghost
                })
                .disabled(*disabled);
            trigger = match (avatar, icon) {
                (Some(pubkey), _) => {
                    trigger.child(avatar_of(pubkey, buzz_ui::AvatarSize::Small, draw, cx))
                }
                (None, Some(icon)) => trigger.child(toned_icon(icon, *icon_tone, &c)),
                (None, None) => trigger,
            };
            if let Some(tooltip) = tooltip {
                trigger = trigger.tooltip(tooltip.clone());
            }
            let toggle = key.clone();
            let trigger = match label {
                // A labelled menu opens from an ordinary button.
                Some(label) => Button::new(key.clone(), label.clone())
                    .disabled(*disabled)
                    .on_click(move |_, _, cx| {
                        set_open((!is_open(&toggle)).then(|| toggle.clone()), cx)
                    })
                    .into_any_element(),
                None => trigger
                    .on_click(move |_, _, cx| {
                        set_open((!is_open(&toggle)).then(|| toggle.clone()), cx)
                    })
                    .into_any_element(),
            };
            let mut place = div().relative().flex_none().child(trigger);
            if is_open(&key)
                && !*disabled
                && let Some(window) = draw.window
            {
                let mut menu = Menu::new();
                if let Some(title) = title {
                    menu = menu.child(MenuLabel::new(title.clone()));
                }
                for (index, option) in options.iter().take(200).enumerate() {
                    let leading = match (&option.avatar, &option.icon) {
                        (Some(pubkey), _) => Some(
                            avatar_of(pubkey, buzz_ui::AvatarSize::Small, draw, cx)
                                .into_any_element(),
                        ),
                        (None, Some(icon)) => {
                            Some(toned_icon(icon, option.icon_tone, &c).into_any_element())
                        }
                        (None, None) => None,
                    };
                    let send = draw.send.clone();
                    let (id, chosen) = (id.clone(), option.value.clone());
                    let mut item = MenuItem::new(
                        SharedString::from(format!("{key}-option-{index}")),
                        option.label.clone(),
                    )
                    .checked(value.as_ref() == Some(&option.value))
                    .disabled(option.disabled)
                    .on_click(move |_, _, cx| {
                        set_open(None, cx);
                        send(&id, "change", Some(json!(chosen)), cx);
                    });
                    if let Some(leading) = leading {
                        item = item.leading(leading);
                    }
                    menu = menu.child(item);
                }
                place = place.child(overlay::dropdown(
                    window,
                    overlay::Align::End,
                    menu,
                    Rc::new(|_, cx| set_open(None, cx)),
                ));
            }
            place.into_any_element()
        }
        Node::Select {
            id,
            value,
            placeholder,
            options,
            disabled,
            width,
        } => {
            let key = element_id("select");
            let shown = options
                .iter()
                .find(|option| Some(&option.value) == value.as_ref())
                .map(|option| option.label.clone());
            let mut select = Select::new(key.clone())
                .surface(buzz_ui::Surface::Floating)
                .disabled(*disabled);
            if let Some(shown) = shown {
                select = select.value(shown);
            }
            if let Some(placeholder) = placeholder {
                select = select.placeholder(placeholder.clone());
            }
            let toggle = key.clone();
            let select = select.on_click(move |_, _, cx| {
                set_open((!is_open(&toggle)).then(|| toggle.clone()), cx)
            });
            let open = is_open(&key) && !*disabled;
            let place = match draw.window {
                Some(window) => {
                    let send = draw.send.clone();
                    let (id, value, options) = (id.clone(), value.clone(), options.clone());
                    let key = key.clone();
                    overlay::select_menu(
                        select,
                        open,
                        overlay::Align::Start,
                        Some(rems(16.)),
                        move || {
                            let mut menu = Menu::new();
                            for (index, option) in options.into_iter().take(200).enumerate() {
                                let send = send.clone();
                                let id = id.clone();
                                let chosen = option.value.clone();
                                menu = menu.child(
                                    MenuItem::new(
                                        SharedString::from(format!("{key}-option-{index}")),
                                        option.label.clone(),
                                    )
                                    .checked(value.as_ref() == Some(&option.value))
                                    .disabled(option.disabled)
                                    .on_click(
                                        move |_, _, cx| {
                                            set_open(None, cx);
                                            send(&id, "change", Some(json!(chosen)), cx);
                                        },
                                    ),
                                );
                            }
                            menu.into_any_element()
                        },
                        Rc::new(|_, cx| set_open(None, cx)),
                        window,
                    )
                }
                None => div().child(select),
            };
            match width {
                Some(width) => place.w(px((*width).clamp(80, 1200) as f32)),
                None => place,
            }
            .into_any_element()
        }
        Node::Popover {
            icon,
            tooltip,
            title,
            description,
            ..
        } => {
            let key = element_id("popover");
            let mut trigger = IconButton::new(
                key.clone(),
                super::icon(Some(icon.as_deref().unwrap_or("Info"))),
            )
            .size(IconButtonSize::Sm);
            if let Some(tooltip) = tooltip {
                trigger = trigger.tooltip(tooltip.clone());
            }
            let toggle = key.clone();
            let trigger = trigger.on_click(move |_, _, cx| {
                set_open((!is_open(&toggle)).then(|| toggle.clone()), cx)
            });
            let mut place = div().relative().flex_none().child(trigger);
            if is_open(&key)
                && let Some(window) = draw.window
            {
                let mut popover = Popover::new();
                if let Some(title) = title {
                    popover = popover.title(title.clone());
                }
                if let Some(description) = description {
                    popover = popover.description(description.clone());
                }
                place = place.child(overlay::dropdown(
                    window,
                    overlay::Align::End,
                    popover,
                    Rc::new(|_, cx| set_open(None, cx)),
                ));
            }
            place.into_any_element()
        }
        Node::Document { text, html, base } => {
            let Some(media) = media_of(draw, cx) else {
                return div().text_body_sm().child(text.clone()).into_any_element();
            };
            let body = document(text, html.as_deref(), base.as_deref().unwrap_or_default());
            let drawn = crate::ui::document::view::draw(
                &body,
                &crate::ui::document::view::Context {
                    id: element_id("document"),
                    media,
                },
                cx,
            );
            div()
                .w_full()
                .min_w_0()
                .pt(px(drawn.top))
                .pb(px(drawn.bottom))
                .child(drawn.element)
                .into_any_element()
        }
        Node::Collapsible {
            id,
            open,
            marker,
            marker_tooltip,
            header,
            preview,
            fit,
            expandable,
            children: body,
            below,
            actions,
            footer,
        } => {
            let key = element_id("collapsible");
            let width = width_of(&key);
            // The header's text width, less the room for the caret.
            let fits = fit.as_deref().is_some_and(|text| {
                draw.window
                    .is_some_and(|window| fits_one_line(text, width - 24., window))
            });
            let expandable = *expandable && !(fit.is_some() && fits);
            let open = expandable && *open;
            let toggle = {
                let send = draw.send.clone();
                let id = id.clone();
                Rc::new(move |cx: &mut App| send(&id, "toggle", Some(json!(!open)), cx))
            };
            let group: SharedString = format!("{key}-group").into();
            let mut marker_place = div()
                .id(SharedString::from(format!("{key}-marker")))
                .flex()
                .flex_none()
                .justify_center()
                .w(px(32.))
                .children(children(marker, &format!("{path}-m"), draw, cx));
            if expandable {
                let toggle = toggle.clone();
                marker_place = marker_place
                    .cursor_pointer()
                    .on_click(move |_, _, cx| toggle(cx));
                if let Some(tooltip) = marker_tooltip {
                    marker_place = marker_place.tooltip(buzz_ui::Tooltip::text(tooltip.clone()));
                }
            }
            let has_body = !body.is_empty();
            // Room at the end for the caret and the actions.
            let mut head = div()
                .id(SharedString::from(format!("{key}-header")))
                .relative()
                .min_w_0()
                .child(div().pr(px(24.)).children(children(
                    header,
                    &format!("{path}-h"),
                    draw,
                    cx,
                )));
            if expandable && !open {
                head = head.child(
                    div()
                        .pt(px(4.))
                        .pr(px(24.))
                        .text_body_sm()
                        .truncate()
                        .child(preview.clone().unwrap_or_default()),
                );
            }
            if !expandable && has_body {
                head = head.child(
                    div()
                        .pt(px(4.))
                        .pr(px(24.))
                        .text_body_sm()
                        .children(children(body, &format!("{path}-b"), draw, cx)),
                );
            }
            if expandable {
                let toggle = toggle.clone();
                head = head
                    .cursor_pointer()
                    .on_click(move |_, _, cx| toggle(cx))
                    .child(
                        div()
                            .absolute()
                            .top(px(4.))
                            .right(px(3.))
                            .text_color(c.text_subtle)
                            .child(
                                Icon::new(if open {
                                    IconName::CaretUp
                                } else {
                                    IconName::CaretDown
                                })
                                .size(14.),
                            ),
                    );
            }
            if !actions.is_empty() {
                let at_top = !expandable || !has_body;
                let place = div().absolute().right_0();
                let place = if at_top {
                    place.top_0()
                } else {
                    place.bottom_0()
                };
                head = head.child(
                    place
                        .opacity(0.)
                        .group_hover(group.clone(), |style| style.opacity(1.))
                        .flex()
                        .gap(space::S1)
                        .children(children(actions, &format!("{path}-a"), draw, cx)),
                );
            }
            let measure_key = key.clone();
            let content = div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    canvas(
                        move |bounds, _, cx| remember_width(&measure_key, bounds.size.width, cx),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(px(1.)),
                )
                .child(head)
                .when(!open && !below.is_empty(), |content| {
                    content.child(div().mt(px(8.)).children(children(
                        below,
                        &format!("{path}-x"),
                        draw,
                        cx,
                    )))
                })
                .when(open, |content| {
                    content
                        .child(
                            div()
                                .pt(px(4.))
                                .pr(px(24.))
                                .text_body_sm()
                                .children(children(body, &format!("{path}-b"), draw, cx)),
                        )
                        .when(!footer.is_empty(), |content| {
                            content.child(div().mt(px(8.)).flex().children(children(
                                footer,
                                &format!("{path}-f"),
                                draw,
                                cx,
                            )))
                        })
                });
            div()
                .id(SharedString::from(format!("{key}-message")))
                .group(group)
                .flex()
                .gap(px(12.))
                .py(px(8.))
                .map(|row| {
                    if has_body {
                        row.items_start()
                    } else {
                        row.items_center()
                    }
                })
                .child(marker_place)
                .child(content)
                .into_any_element()
        }
        Node::Thumbnails { images, .. } => {
            let Some(media) = media_of(draw, cx) else {
                return div().into_any_element();
            };
            let key = element_id("thumbnails");
            let available = width_of(&key).max(40.);
            let capacity = (((available + 4.) / 44.).floor() as usize).max(1);
            let visible = images.len().min(capacity);
            let hidden = images.len() - visible;
            let measure_key = key.clone();
            div()
                .relative()
                .w_full()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .child(
                    canvas(
                        move |bounds, _, cx| remember_width(&measure_key, bounds.size.width, cx),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(px(1.)),
                )
                .children(images.iter().take(visible).enumerate().map(|(index, src)| {
                    let attachment = buzz_relay_client::fold::Attachment {
                        url: src.clone(),
                        kind: buzz_relay_client::fold::AttachmentKind::Image,
                        mime: None,
                        size: None,
                        name: None,
                        duration: None,
                        waveform: None,
                        voice_note: false,
                        dimensions: None,
                        blurhash: None,
                        preview_url: None,
                        insecure: false,
                    };
                    let opener = media.clone();
                    div()
                        .id(SharedString::from(format!("{key}-{index}")))
                        .relative()
                        .flex_none()
                        .size(px(40.))
                        .rounded(px(12.))
                        .overflow_hidden()
                        .bg(c.surface_inset)
                        .cursor(CursorStyle::PointingHand)
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            crate::ui::media::open_media(attachment.clone(), &opener, window, cx)
                        })
                        .child(crate::ui::media::thumbnail(src, None, &media))
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .rounded(px(12.))
                                .border_1()
                                .border_color(c.border_standard),
                        )
                        .when(index + 1 == visible && hidden > 0, |tile| {
                            tile.child(
                                div()
                                    .absolute()
                                    .right_0()
                                    .bottom_0()
                                    .px(px(4.))
                                    .py(px(2.))
                                    .rounded_tl(px(12.))
                                    .bg(c.surface_panel)
                                    .text_caption()
                                    .text_color(c.text_standard)
                                    .child(format!("+{hidden}")),
                            )
                        })
                }))
                .into_any_element()
        }
        Node::Tabs { id, value, options } => {
            let mut tabs = buzz_ui::Tabs::new(buzz_ui::TabVariant::Panel);
            for (index, option) in options.iter().take(20).enumerate() {
                let send = draw.send.clone();
                let (id, chosen) = (id.clone(), option.value.clone());
                tabs = tabs.tab(
                    buzz_ui::Tab::new(
                        SharedString::from(format!("{}-{index}", element_id("tabs"))),
                        option.label.clone(),
                    )
                    .selected(value.as_ref() == Some(&option.value))
                    .on_click(move |_, _, cx| send(&id, "change", Some(json!(chosen)), cx)),
                );
            }
            tabs.into_any_element()
        }
        Node::Ring { segments, size } => {
            let total: f32 = segments.iter().map(|segment| segment.value.max(0.)).sum();
            let arcs: Vec<(f32, Hsla)> = segments
                .iter()
                .filter(|segment| segment.value > 0. && total > 0.)
                .map(|segment| {
                    let color = match segment.tone {
                        Tone::Danger => c.text_danger,
                        Tone::Warning => c.text_warning,
                        Tone::Success | Tone::Complete => c.text_success,
                        Tone::Accent | Tone::Progress => c.text_accent,
                        Tone::Default | Tone::Subtle => c.border_standard,
                    };
                    (segment.value / total, color)
                })
                .collect();
            let size = size.unwrap_or(32).clamp(16, 128) as f32;
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let scale = f32::from(bounds.size.width) / 32.;
                    let center = bounds.center();
                    let radius = 13. * scale;
                    let mut start = 0f32;
                    for (fraction, color) in &arcs {
                        let sweep = fraction * std::f32::consts::TAU;
                        let steps = ((sweep / 0.05).ceil() as usize).max(2);
                        let mut path = PathBuilder::stroke(px(3. * scale));
                        for step in 0..=steps {
                            let angle = start + sweep * step as f32 / steps as f32
                                - std::f32::consts::FRAC_PI_2;
                            let at = point(
                                center.x + px(radius * angle.cos()),
                                center.y + px(radius * angle.sin()),
                            );
                            if step == 0 {
                                path.move_to(at);
                            } else {
                                path.line_to(at);
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, *color);
                        }
                        start += sweep;
                    }
                },
            )
            .flex_none()
            .size(px(size))
            .into_any_element()
        }
        Node::Empty {
            icon,
            title,
            description,
        } => div()
            .w_full()
            .border_1()
            .border_color(c.border_standard)
            .rounded(radius::CONTAINER)
            .flex()
            .flex_col()
            .items_center()
            .gap(space::S4)
            .py(space::S8)
            .px(space::S6)
            .text_center()
            .children(icon.as_deref().map(|name| {
                Icon::new(super::icon(Some(name)))
                    .size(32.)
                    .color(c.text_subtle)
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S2)
                    .max_w(rems(30.))
                    .child(div().text_label_sm().child(title.clone()))
                    .children(description.clone().map(|description| {
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child(description)
                    })),
            )
            .into_any_element(),
        Node::Divider => Divider::horizontal().into_any_element(),
        Node::Code {
            lines,
            wrap,
            min_width,
        } => {
            let block = code(lines, *wrap, &c);
            match min_width {
                Some(width) => div()
                    .id(element_id("code"))
                    .w_full()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .child(block.min_w(px((*width).min(2400) as f32)))
                    .into_any_element(),
                None if !*wrap => div()
                    .id(element_id("code"))
                    .w_full()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .child(block)
                    .into_any_element(),
                None => block.into_any_element(),
            }
        }
        Node::Scroll {
            children: nodes,
            max_height,
            padding,
        } => {
            let id = element_id("scroll");
            let handle = scroll_handle(&id);
            padding_of(div(), *padding)
                .id(id)
                .flex()
                .flex_col()
                .max_h(px((*max_height).clamp(40, 4000) as f32))
                .overflow_y_scroll()
                .track_scroll(&handle)
                .on_scroll_wheel(chain_scroll(handle))
                .children(children(nodes, path, draw, cx))
                .into_any_element()
        }
        Node::Spinner => Spinner::new().size(16.).into_any_element(),
        Node::Unknown => div().into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn inputs_are_found_in_order() {
        let tree = Node::parse(json!({
            "type": "column",
            "children": [
                { "type": "input", "id": "a" },
                { "type": "card", "children": [{ "type": "input", "id": "b", "version": 3 }] },
                { "type": "text", "text": "x" },
            ],
        }))
        .unwrap();
        let found: Vec<_> = inputs(&tree)
            .iter()
            .map(|input| (input.id.to_owned(), input.version))
            .collect();
        assert_eq!(found, [("a".to_owned(), 0), ("b".to_owned(), 3)]);
    }
}
