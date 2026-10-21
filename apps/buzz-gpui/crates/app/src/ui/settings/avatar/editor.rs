//! The "Edit avatar" popover (`AvatarDraft` in `AvatarEditor.tsx`): Image,
//! Emoji and Background tabs, a drop zone, the picture URL, "Remove
//! avatar" and "Done". Emoji avatars are drawn here and uploaded as PNG.

use super::raster::{self, Hsv};
use crate::community::Community;
use crate::ui::emoji_picker::{Candidate, most_used, search as search_emoji};

/// emoji-mart's defaults for an empty history, one row.
const FREQUENT_DEFAULTS: [&str; 6] = ["👍", "😀", "😘", "😍", "😆", "😜"];
use buzz_relay_client::account::picture_error;
use buzz_relay_client::upload::{self, UploadFile};
use buzz_ui::{
    ActiveTheme, Button, ButtonVariant, Icon, IconName, SearchField, Tab, TabVariant, Tabs,
    TextField, Tooltip, TypeStyled, tokens::radius, tokens::shadow,
};
use emojis::{Emoji, Group, SkinTone, UnicodeVersion};
use gpui_kit::component::input::{Escape, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, LazyLock};

/// The 23 background swatches.
const COLORS: [&str; 23] = [
    "#FFFFFF", "#FFF4CC", "#FFE75C", "#FFB84D", "#FF8652", "#F6534F", "#FF6B9A", "#FB60C4",
    "#D66BFF", "#B141FF", "#7C5CFF", "#476CFF", "#3399FF", "#63C6F2", "#41EBC1", "#2ED3A2",
    "#73EF75", "#9FE870", "#C7D36F", "#CCCCCC", "#8A8F98", "#4B5563", "#000000",
];
const FIRST_EMOJI: &str = "😀";
const FIRST_COLOR: &str = "#FFF4CC";
/// The popover: 22.5rem wide with 12px padding.
pub const WIDTH: f32 = 360.;
/// emoji-mart's slots: 48px buttons, six to a line.
const CELL: f32 = 48.;
const PER_LINE: usize = 6;
/// The bundled font draws emoji through Unicode 16.
const NEWEST: UnicodeVersion = UnicodeVersion::new(16, 0);

/// Done: the picture to use, empty to remove the avatar.
pub struct AvatarDone(pub String);
/// Escape: close without a change.
pub struct AvatarDismissed;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Image,
    Emoji,
    Background,
}

/// What the avatar shows while the editor is open.
pub enum Preview {
    Picture(String),
    Emoji {
        art: Option<Arc<RenderImage>>,
        color: Hsla,
    },
}

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
}

impl Category {
    /// Every category but Frequent, whose emoji depend on use.
    const ALL: [Category; 8] = [
        Category::People,
        Category::Nature,
        Category::Food,
        Category::Activity,
        Category::Travel,
        Category::Objects,
        Category::Symbols,
        Category::Flags,
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
        }
    }

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
        }
    }

    fn groups(self) -> &'static [Group] {
        match self {
            Category::Frequent => &[],
            Category::People => &[Group::SmileysAndEmotion, Group::PeopleAndBody],
            Category::Nature => &[Group::AnimalsAndNature],
            Category::Food => &[Group::FoodAndDrink],
            Category::Activity => &[Group::Activities],
            Category::Travel => &[Group::TravelAndPlaces],
            Category::Objects => &[Group::Objects],
            Category::Symbols => &[Group::Symbols],
            Category::Flags => &[Group::Flags],
        }
    }
}

const TONES: [(Option<SkinTone>, &str); 6] = [
    (None, "✋"),
    (Some(SkinTone::Light), "✋🏻"),
    (Some(SkinTone::MediumLight), "✋🏼"),
    (Some(SkinTone::Medium), "✋🏽"),
    (Some(SkinTone::MediumDark), "✋🏾"),
    (Some(SkinTone::Dark), "✋🏿"),
];

/// Every emoji the font draws, by category.
static CATEGORIES: LazyLock<Vec<(Category, Vec<&'static Emoji>)>> = LazyLock::new(|| {
    Category::ALL
        .iter()
        .map(|category| {
            let emoji = emojis::iter()
                .filter(|emoji| {
                    category.groups().contains(&emoji.group()) && emoji.unicode_version() <= NEWEST
                })
                .collect();
            (*category, emoji)
        })
        .collect()
});

#[derive(Clone)]
enum Row {
    Header(&'static str),
    Cells(Vec<&'static Emoji>),
    Empty,
}

/// The custom colour panel's state while open.
struct Custom {
    hsv: Hsv,
    spectrum: Rc<Cell<Bounds<Pixels>>>,
    hue: Rc<Cell<Bounds<Pixels>>>,
    spectrum_focus: FocusHandle,
    hue_focus: FocusHandle,
}

pub struct AvatarEditor {
    /// Where uploads go; without one only a URL can be used.
    community: Option<Entity<Community>>,
    /// The picture the form had when the editor opened.
    value: String,
    picture: Entity<InputState>,
    mode: Mode,
    emoji: String,
    /// The emoji drawn for the preview.
    art: Option<Arc<RenderImage>>,
    color: String,
    custom: Option<Custom>,
    busy: bool,
    error: Option<SharedString>,
    dragging: bool,
    search: Entity<InputState>,
    list: ListState,
    rows: Rc<Vec<Row>>,
    sections: Vec<(Category, usize)>,
    active: Category,
    tone: usize,
    tones_open: bool,
    /// The custom colour button's conic gradient and the hue track.
    conic: Option<Arc<RenderImage>>,
    hue_track: Option<Arc<RenderImage>>,
    _upload: Option<Task<()>>,
    _art: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AvatarDone> for AvatarEditor {}
impl EventEmitter<AvatarDismissed> for AvatarEditor {}

impl AvatarEditor {
    pub fn new(
        value: String,
        community: Option<Entity<Community>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picture = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste an image URL")
                .default_value(value.clone())
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let list = ListState::new(0, ListAlignment::Top, px(400.));
        let weak = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _, cx| {
            let first = event.visible_range.start;
            let _ = weak.update(cx, |this, cx| {
                let active = this
                    .sections
                    .iter()
                    .rev()
                    .find(|(_, start)| *start <= first)
                    .map(|(category, _)| *category);
                if let Some(active) = active
                    && active != this.active
                {
                    this.active = active;
                    cx.notify();
                }
            });
        });
        let _subscriptions = vec![
            cx.subscribe_in(&picture, window, |this, _, event: &InputEvent, _, cx| {
                match event {
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    // Enter applies the URL, as Done does.
                    InputEvent::PressEnter { .. } => {
                        let picture = this.picture.read(cx).value().to_string();
                        if !this.busy && picture_error(&picture).is_none() {
                            cx.emit(AvatarDone(picture));
                        }
                    }
                    _ => {}
                }
            }),
            cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.rebuild(cx);
                }
            }),
        ];
        let mut editor = Self {
            community,
            value,
            picture,
            mode: Mode::Image,
            emoji: FIRST_EMOJI.into(),
            art: None,
            color: FIRST_COLOR.into(),
            custom: None,
            busy: false,
            error: None,
            dragging: false,
            search,
            list,
            rows: Rc::default(),
            sections: Vec::new(),
            active: Category::Frequent,
            tone: 0,
            tones_open: false,
            conic: raster::render_image(&raster::conic(72)),
            hue_track: raster::render_image(&raster::hue_track(320, 40)),
            _upload: None,
            _art: None,
            _subscriptions,
        };
        editor.rebuild(cx);
        editor.draw_emoji(cx);
        editor
    }

    /// What the avatar shows meanwhile: the emoji over its background on
    /// the Emoji and Background tabs, else the picture being edited.
    pub fn preview(&self, cx: &App) -> Preview {
        match self.mode {
            Mode::Image => Preview::Picture(self.picture.read(cx).value().to_string()),
            _ => Preview::Emoji {
                art: self.art.clone(),
                color: hex_color(&self.color),
            },
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.picture.update(cx, |state, cx| state.focus(window, cx));
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.mode = mode;
        self.custom = None;
        self.error = None;
        cx.notify();
    }

    /// Draws the chosen emoji off the UI thread for the preview.
    fn draw_emoji(&mut self, cx: &mut Context<Self>) {
        let emoji = self.emoji.clone();
        let drawn = cx.background_spawn(async move {
            raster::emoji_layer(&emoji).and_then(|layer| raster::render_image(&layer))
        });
        self._art = Some(cx.spawn(async move |this, cx| {
            let art = drawn.await;
            let _ = this.update(cx, |this, cx| {
                this.art = art;
                cx.notify();
            });
        }));
    }

    fn pick_emoji(&mut self, emoji: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.emoji = emoji;
        self.draw_emoji(cx);
        cx.notify();
    }

    fn set_color(&mut self, color: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.color = color;
        cx.notify();
    }

    /// Uploads a prepared file to the community; `apply` finishes with its
    /// URL, otherwise the URL goes into the picture field.
    fn upload(
        &mut self,
        prepare: impl FnOnce() -> Result<UploadFile, String> + Send + 'static,
        apply: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(community) = &self.community else {
            return;
        };
        if self.busy {
            return;
        }
        let media = community.read(cx).session.media().clone();
        self.busy = true;
        self.error = None;
        let prepared = cx.background_spawn(async move { prepare() });
        self._upload = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match prepared.await {
                Ok(file) => upload::upload(&media, file)
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|blob| {
                        if blob.mime.starts_with("image/") {
                            Ok(blob.url)
                        } else {
                            Err("The server did not return an avatar image.".to_string())
                        }
                    }),
                Err(error) => Err(error),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(url) if apply => cx.emit(AvatarDone(url)),
                    Ok(url) => this
                        .picture
                        .update(cx, |state, cx| state.set_value(url, window, cx)),
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn upload_path(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = Mode::Image;
        self.custom = None;
        self.upload(move || super::read(&path), false, window, cx);
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.community.is_none() {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose an avatar".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                let _ = this.update_in(cx, |this, window, cx| this.upload_path(path, window, cx));
            }
        })
        .detach();
    }

    fn done(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.mode == Mode::Image {
            let picture = self.picture.read(cx).value().to_string();
            if picture_error(&picture).is_none() {
                cx.emit(AvatarDone(picture));
            }
            return;
        }
        let (emoji, color) = (self.emoji.clone(), self.color.clone());
        self.upload(
            move || {
                Ok(UploadFile {
                    name: "avatar.png".into(),
                    mime: "image/png".into(),
                    bytes: raster::emoji_png(&emoji, &color)?,
                })
            },
            true,
            window,
            cx,
        );
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_string();
        let mut rows = Vec::new();
        self.sections.clear();
        let push = |rows: &mut Vec<Row>, cells: Vec<&'static Emoji>| {
            for chunk in cells.chunks(PER_LINE) {
                rows.push(Row::Cells(chunk.to_vec()));
            }
        };
        if query.trim().is_empty() {
            // emoji-mart's Frequent row, at most two lines (`maxFrequentRows`):
            // the reactions used most in this community, then its defaults.
            let mut frequent: Vec<&'static Emoji> = match &self.community {
                Some(community) => {
                    let partition = community.read(cx).partition();
                    most_used(&partition, PER_LINE * 2, cx)
                        .iter()
                        .filter_map(|used| emojis::get(used))
                        .collect()
                }
                None => Vec::new(),
            };
            for default in FREQUENT_DEFAULTS.iter().filter_map(|d| emojis::get(d)) {
                if frequent.len() < PER_LINE && !frequent.contains(&default) {
                    frequent.push(default);
                }
            }
            self.sections.push((Category::Frequent, rows.len()));
            rows.push(Row::Header(Category::Frequent.title()));
            push(&mut rows, frequent);
            for (category, emoji) in CATEGORIES.iter() {
                self.sections.push((*category, rows.len()));
                rows.push(Row::Header(category.title()));
                push(&mut rows, emoji.clone());
            }
        } else {
            let found: Vec<&'static Emoji> = search_emoji(&query, &[], 240)
                .into_iter()
                .filter_map(|candidate| match candidate {
                    Candidate::Unicode(emoji) if emoji.unicode_version() <= NEWEST => Some(emoji),
                    _ => None,
                })
                .collect();
            rows.push(Row::Header("Search results"));
            if found.is_empty() {
                rows.push(Row::Empty);
            }
            push(&mut rows, found);
        }
        self.rows = Rc::new(rows);
        self.list.reset(self.rows.len());
        self.active = Category::Frequent;
        cx.notify();
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

    fn open_custom(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.custom = Some(Custom {
            hsv: raster::hsv_from_hex(&self.color),
            spectrum: Rc::default(),
            hue: Rc::default(),
            spectrum_focus: cx.focus_handle(),
            hue_focus: cx.focus_handle(),
        });
        cx.notify();
    }

    /// Applies a custom colour change.
    fn adjust(&mut self, change: impl FnOnce(&mut Hsv), cx: &mut Context<Self>) {
        let Some(custom) = &mut self.custom else {
            return;
        };
        change(&mut custom.hsv);
        self.color = raster::hex_from_hsv(custom.hsv);
        cx.notify();
    }

    /// Saturation and brightness from a point on the spectrum.
    fn spectrum_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.custom.as_ref().map(|custom| custom.spectrum.get()) else {
            return;
        };
        let x = (position.x - bounds.origin.x) / bounds.size.width.max(px(1.));
        let y = (position.y - bounds.origin.y) / bounds.size.height.max(px(1.));
        self.adjust(
            |hsv| {
                hsv.saturation = clamp(x as f64 * 100.);
                hsv.value = clamp((1. - y as f64) * 100.);
            },
            cx,
        );
    }

    fn hue_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.custom.as_ref().map(|custom| custom.hue.get()) else {
            return;
        };
        let x = (position.x - bounds.origin.x) / bounds.size.width.max(px(1.));
        self.adjust(|hsv| hsv.hue = (x as f64).clamp(0., 1.) * 360., cx);
    }
}

fn clamp(value: f64) -> f64 {
    value.clamp(0., 100.)
}

fn hex_color(hex: &str) -> Hsla {
    let [r, g, b] = raster::parse_hex(hex).unwrap_or([255, 255, 255]);
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// Records an element's bounds as it is laid out.
fn bounds_probe(slot: Rc<Cell<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(move |bounds, _, _| slot.set(bounds), |_, _, _, _| {})
        .absolute()
        .size_full()
}

impl AvatarEditor {
    fn image_panel(&self, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let picture = self.picture.read(cx).value().to_string();
        let error = picture_error(&picture).map(|error| SharedString::from(error.to_string()));
        let upload = !self.busy && self.community.is_some();
        let label = if self.busy {
            "Uploading…"
        } else if self.dragging {
            "Drop image here"
        } else {
            "Drop or browse"
        };
        div()
            .flex()
            .flex_col()
            .gap(rems(0.75))
            .pt(rems(0.5))
            .child(
                div()
                    .id("avatar-dropzone")
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(rems(0.5))
                    .min_h(rems(5.))
                    .rounded(radius::CONTROL)
                    .bg(c.surface_inset)
                    .text_color(c.text_subtle)
                    .text_body_sm()
                    .when(upload, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(c.affordance_subtle_hover))
                            .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx)))
                    })
                    .when(!upload, |this| this.opacity(0.5))
                    .child(Icon::new(IconName::CloudUpload).size(24.))
                    .child(label),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(
                        TextField::new(&self.picture)
                            .leading(Icon::new(IconName::Link).size(18.))
                            .invalid(error.is_some())
                            .disabled(self.busy),
                    )
                    .children(
                        error.map(|error| {
                            div().text_caption().text_color(c.text_danger).child(error)
                        }),
                    ),
            )
    }

    fn emoji_panel(&self, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let rows = self.rows.clone();
        let tone = TONES[self.tone].0;
        let weak = cx.entity().downgrade();
        let grid = list(self.list.clone(), move |index, _, cx| {
            let Some(row) = rows.get(index) else {
                return div().into_any_element();
            };
            let c = cx.colors();
            match row {
                Row::Header(title) => div()
                    .px(px(4.))
                    .pt(px(8.))
                    .pb(px(4.))
                    .text_caption()
                    .text_color(c.text_subtle)
                    .child(*title)
                    .into_any_element(),
                Row::Empty => div()
                    .px(px(4.))
                    .py(px(24.))
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child("No emoji found")
                    .into_any_element(),
                Row::Cells(cells) => div()
                    .flex()
                    .children(cells.iter().map(|emoji| {
                        let text = match tone {
                            Some(tone) => emoji
                                .with_skin_tone(tone)
                                .map_or(emoji.as_str(), |toned| toned.as_str()),
                            None => emoji.as_str(),
                        };
                        let weak = weak.clone();
                        let picked = text.to_string();
                        div()
                            .id(SharedString::from(format!("avatar-emoji-{text}")))
                            .size(px(CELL))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded(px(12.))
                            .cursor_pointer()
                            .hover(|s| s.bg(c.affordance_popover_selected))
                            .tooltip(Tooltip::text(SharedString::from(emoji.name())))
                            .text_size(px(32.))
                            .line_height(px(40.))
                            .child(text)
                            .on_click(move |_, _, cx| {
                                let picked = picked.clone();
                                let _ = weak.update(cx, |this, cx| this.pick_emoji(picked, cx));
                            })
                            .into_any_element()
                    }))
                    .into_any_element(),
            }
        })
        .flex_1()
        .w_full();
        let tone_control = if self.tones_open {
            div()
                .flex()
                .gap(px(2.))
                .children((0..TONES.len()).map(|index| self.tone_swatch(index, cx)))
                .into_any_element()
        } else {
            div()
                .id("avatar-emoji-tone")
                .child(self.tone_swatch(self.tone, cx))
                .tooltip(Tooltip::text("Skin tone"))
                .into_any_element()
        };
        let nav = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(44.))
            .border_t_1()
            .border_color(c.border_standard)
            .children(self.sections.iter().map(|(category, _)| {
                let category = *category;
                let active = category == self.active;
                div()
                    .id(SharedString::from(format!(
                        "avatar-nav-{}",
                        category.title()
                    )))
                    .size(px(30.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .text_color(if active {
                        c.text_standard
                    } else {
                        c.text_subtle
                    })
                    .when(active, |d| d.bg(c.affordance_subtle))
                    .hover(|s| s.bg(c.affordance_subtle_hover))
                    .tooltip(Tooltip::text(category.title()))
                    .child(Icon::new(category.icon()).size(16.))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.jump(category, window, cx)),
                    )
            }));
        // `.emojiMart`: min(21.75rem, 40vh) tall.
        div()
            .flex()
            .flex_col()
            .h(rems(21.75))
            .pt(rems(0.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .pb(px(4.))
                    .child(
                        div()
                            .flex_1()
                            .child(SearchField::new(&self.search).capsule()),
                    )
                    .child(tone_control),
            )
            .child(grid)
            .child(nav)
    }

    fn tone_swatch(&self, index: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let c = *cx.colors();
        div()
            .id(("avatar-tone", index))
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .when(index == self.tone, |d| d.bg(c.affordance_subtle))
            .hover(|s| s.bg(c.affordance_subtle_hover))
            .text_size(px(16.))
            .child(TONES[index].1)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.tones_open {
                    this.tone = index;
                    this.tones_open = false;
                } else {
                    this.tones_open = true;
                }
                cx.notify();
            }))
    }

    fn background_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(custom) = &self.custom {
            return self.custom_panel(custom, cx).into_any_element();
        }
        let c = *cx.colors();
        let current = self.color.to_uppercase();
        let swatch = |id: ElementId, selected: bool| {
            div()
                .id(id)
                .size(rems(2.25))
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(c.border_standard)
                .cursor_pointer()
                .when(selected, |this| {
                    // A 2px outline 2px off the swatch.
                    this.shadow(vec![
                        BoxShadow {
                            color: c.surface_popover,
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(0.),
                            spread_radius: px(2.),
                            inset: false,
                        },
                        BoxShadow {
                            color: c.text_standard,
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(0.),
                            spread_radius: px(4.),
                            inset: false,
                        },
                    ])
                })
        };
        let cells = COLORS
            .iter()
            .map(|hex| {
                let hex = *hex;
                swatch(
                    ElementId::Name(format!("avatar-swatch-{hex}").into()),
                    current == hex,
                )
                .bg(hex_color(hex))
                .tooltip(Tooltip::text(format!("Use {hex} background")))
                .on_click(cx.listener(move |this, _, _, cx| this.set_color(hex.into(), cx)))
                .into_any_element()
            })
            .chain(std::iter::once(
                swatch(
                    "avatar-custom-color".into(),
                    !COLORS.contains(&current.as_str()),
                )
                .overflow_hidden()
                .children(
                    self.conic
                        .clone()
                        .map(|conic| img(conic).size_full().rounded_full()),
                )
                .tooltip(Tooltip::text("Custom background color"))
                .on_click(cx.listener(|this, _, _, cx| this.open_custom(cx)))
                .into_any_element(),
            ))
            .collect::<Vec<_>>();
        // `repeat(auto-fit, minmax(2.75rem, 1fr))`: six columns here.
        let mut grid = div().flex().flex_col().gap(rems(0.75)).py(rems(0.75));
        let mut cells = cells.into_iter().peekable();
        while cells.peek().is_some() {
            let row: Vec<AnyElement> = cells.by_ref().take(6).collect();
            let blanks = 6 - row.len();
            grid = grid.child(
                div()
                    .flex()
                    .gap(rems(0.5))
                    .children(
                        row.into_iter()
                            .map(|cell| div().flex_1().flex().justify_center().child(cell)),
                    )
                    .children((0..blanks).map(|_| div().flex_1())),
            );
        }
        div().pt(rems(0.5)).child(grid).into_any_element()
    }

    fn custom_panel(&self, custom: &Custom, cx: &mut Context<Self>) -> Div {
        let hsv = custom.hsv;
        // `hsl(hue, 100%, 50%)` under the two gradients.
        let pure = hex_color(&raster::hex_from_hsv(Hsv {
            hue: hsv.hue,
            saturation: 100.,
            value: 100.,
        }));
        let thumb_color = hex_color(&raster::hex_from_hsv(hsv));
        let white = hsla(0., 0., 1., 1.);
        let thumb = |color: Hsla| {
            div()
                .absolute()
                .size(rems(2.))
                .rounded_full()
                .border(px(3.))
                .border_color(white)
                .bg(color)
                .shadow(shadow::sm())
        };
        let spectrum = div()
            .id("avatar-spectrum")
            .relative()
            .h(rems(14.))
            .rounded(radius::CONTROL)
            .cursor_crosshair()
            .track_focus(&custom.spectrum_focus)
            .bg(pure)
            .child(bounds_probe(custom.spectrum.clone()))
            .child(
                div()
                    .absolute()
                    .size_full()
                    .rounded(radius::CONTROL)
                    .bg(linear_gradient(
                        90.,
                        linear_color_stop(white, 0.),
                        linear_color_stop(hsla(0., 0., 1., 0.), 1.),
                    )),
            )
            .child(
                div()
                    .absolute()
                    .size_full()
                    .rounded(radius::CONTROL)
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(hsla(0., 0., 0., 0.), 0.),
                        linear_color_stop(hsla(0., 0., 0., 1.), 1.),
                    )),
            )
            .child(
                thumb(thumb_color)
                    .left(relative(hsv.saturation as f32 / 100.))
                    .top(relative(1. - hsv.value as f32 / 100.))
                    .ml(rems(-1.))
                    .mt(rems(-1.)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if let Some(custom) = &this.custom {
                        window.focus(&custom.spectrum_focus, cx);
                    }
                    this.spectrum_at(event.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    this.spectrum_at(event.position, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let change: Option<fn(&mut Hsv)> = match event.keystroke.key.as_str() {
                    "left" => Some(|hsv| hsv.saturation = clamp(hsv.saturation - 1.)),
                    "right" => Some(|hsv| hsv.saturation = clamp(hsv.saturation + 1.)),
                    "down" => Some(|hsv| hsv.value = clamp(hsv.value - 1.)),
                    "up" => Some(|hsv| hsv.value = clamp(hsv.value + 1.)),
                    "home" => Some(|hsv| hsv.saturation = 0.),
                    "end" => Some(|hsv| hsv.saturation = 100.),
                    _ => None,
                };
                if let Some(change) = change {
                    cx.stop_propagation();
                    this.adjust(change, cx);
                }
            }));
        let track = div()
            .id("avatar-hue")
            .relative()
            .h(rems(2.5))
            .px(rems(0.25))
            .rounded_full()
            .cursor_pointer()
            .track_focus(&custom.hue_focus)
            .children(
                self.hue_track
                    .clone()
                    .map(|track| img(track).absolute().inset_0().size_full().rounded_full()),
            )
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(bounds_probe(custom.hue.clone()))
                    .child(
                        thumb(white)
                            .border_0()
                            .left(relative(hsv.hue as f32 / 360.))
                            .top(rems(0.25))
                            .ml(rems(-1.)),
                    ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if let Some(custom) = &this.custom {
                        window.focus(&custom.hue_focus, cx);
                    }
                    this.hue_at(event.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    this.hue_at(event.position, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let step = match event.keystroke.key.as_str() {
                    "left" | "down" => -1.,
                    "right" | "up" => 1.,
                    _ => return,
                };
                cx.stop_propagation();
                this.adjust(|hsv| hsv.hue = (hsv.hue + step).clamp(0., 360.), cx);
            }));
        div()
            .flex()
            .flex_col()
            .gap(rems(1.))
            .p(rems(1.))
            .child(spectrum)
            .child(track)
            .child(
                Button::new("avatar-use-color", "Use color")
                    .full_width()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.custom = None;
                        cx.notify();
                    })),
            )
    }
}

impl Render for AvatarEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let busy = self.busy;
        let picture = self.picture.read(cx).value().to_string();
        let has_community = self.community.is_some();
        let done_disabled = busy
            || match self.mode {
                Mode::Image => picture_error(&picture).is_some(),
                _ => !has_community || self.emoji.trim().is_empty(),
            };
        let tab = |id: &'static str, label: &'static str, mode: Mode, cx: &mut Context<Self>| {
            Tab::new(id, label)
                .selected(self.mode == mode)
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
        };
        let panel = match self.mode {
            Mode::Image => self.image_panel(cx).into_any_element(),
            Mode::Emoji => self.emoji_panel(cx).into_any_element(),
            Mode::Background => self.background_panel(cx),
        };
        let custom_open = self.mode == Mode::Background && self.custom.is_some();
        div()
            .id("avatar-picker")
            .key_context("AvatarEditor")
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                // Escape closes the custom colour first, as its own layer.
                cx.stop_propagation();
                if this.custom.take().is_some() {
                    cx.notify();
                } else {
                    cx.emit(AvatarDismissed);
                }
            }))
            .flex()
            .flex_col()
            .gap(rems(0.75))
            .rounded(radius::CONTROL)
            .border_2()
            .border_color(if self.dragging {
                c.text_standard
            } else {
                transparent_black()
            })
            .when(self.dragging, |this| this.border_dashed())
            .text_color(c.text_standard)
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                    let over = event.bounds.contains(&event.event.position)
                        && !this.busy
                        && this.community.is_some();
                    if this.dragging != over {
                        this.dragging = over;
                        if over {
                            this.mode = Mode::Image;
                            this.custom = None;
                        }
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.dragging = false;
                if let Some(path) = paths.paths().first().cloned()
                    && !this.busy
                    && this.community.is_some()
                {
                    this.upload_path(path, window, cx);
                }
                cx.notify();
            }))
            .child(
                Tabs::new(TabVariant::Panel)
                    .tab(tab("avatar-tab-image", "Image", Mode::Image, cx))
                    .tab(tab("avatar-tab-emoji", "Emoji", Mode::Emoji, cx))
                    .tab(tab(
                        "avatar-tab-background",
                        "Background",
                        Mode::Background,
                        cx,
                    )),
            )
            .child(panel)
            .when(busy, |this| {
                this.child(div().text_body_sm().child("Uploading avatar…"))
            })
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .when(!has_community, |this| {
                this.child(div().text_body_sm().text_color(c.text_subtle).child(
                    "Select a community in the sidebar to upload an image or emoji. \
                             Your local default can use a public HTTPS image URL.",
                ))
            })
            .when(!custom_open, |this| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_between()
                        .gap(rems(0.5))
                        .child(
                            Button::new("avatar-remove", "Remove avatar")
                                .variant(ButtonVariant::Destructive)
                                .disabled((self.value.is_empty() && picture.is_empty()) || busy)
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(AvatarDone(String::new()))),
                                ),
                        )
                        .child(
                            Button::new("avatar-done", "Done")
                                .variant(ButtonVariant::Prominent)
                                .disabled(done_disabled)
                                .on_click(cx.listener(|this, _, window, cx| this.done(window, cx))),
                        ),
                )
            })
    }
}
