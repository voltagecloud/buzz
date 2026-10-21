//! The New agent dialog (buzz-app's `AgentCreateDialog.tsx` and
//! `AgentCreateHeader.tsx`): one card with the avatar and name, which the
//! app owns, and the chosen type's part below them. The avatar is a random
//! pack avatar to start with; Choose avatar swaps the card for the packs,
//! a pack's avatars, or an uploaded image. The footer combines the type
//! and model choice in one menu, with each type's model rows in its
//! submenu.
//!
//! Types do not contribute views here (buzz-app's `Create` is a plugin
//! API): Claude's and Codex's part, the agent's instructions, is built in
//! (`AgentInstructions`). The Host select is this port's own: it picks the
//! machine that keeps the key.

use super::{card, packs, problem};
use crate::agents2::{Agents2, CreateModels, Made, RegisteredType, types};
use crate::hosts::HostConnection;
use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::settings::avatar::{AvatarDismissed, AvatarDone, AvatarEditor};
use buzz_host_rpc::ConnectionState;
use buzz_ui::{
    ActiveTheme, Brand, Button, Dialog, DialogSize, Icon, IconName, Menu, MenuItem, Select,
    Surface, TextArea, TextField, TypeStyled, tokens::radius,
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::Value;
use std::rc::Rc;
use std::time::Duration;

const EXAMPLES: [&str; 4] = [
    "Help me plan my week and prioritize tasks.",
    "Review my writing and suggest clearer wording.",
    "Research a topic and summarize the key ideas.",
    "Help me brainstorm ideas and next steps.",
];
const PLACEHOLDER: &str = "Describe what I should do…";
const EXAMPLE_EVERY: Duration = Duration::from_secs(5);

/// The avatar choices' area in the card: its width and height less the
/// cover's padding (`gridSize`).
const PACK_AREA: (f32, f32) = (848. - 96., 520. - 32.);

/// The columns that make a pack's `count` avatars largest in `width` by
/// `height` with 8px gaps, and that size.
fn pack_grid(count: usize, width: f32, height: f32) -> (usize, f32) {
    let mut best = (1, 0.);
    for columns in 1..=count.max(1) {
        let rows = count.div_ceil(columns);
        let size = ((width - (columns - 1) as f32 * 8.) / columns as f32)
            .min((height - (rows.max(1) - 1) as f32 * 8.) / rows.max(1) as f32);
        if size > best.1 {
            best = (columns, size);
        }
    }
    best
}

pub enum CreateEvent {
    Close,
    Created(String),
}

/// What the card shows.
#[derive(Clone, Debug, PartialEq)]
enum View {
    Selected,
    Collections,
    Pack(String),
    Image,
}

/// The type's starting config, or none when its defaults fail.
fn starting_config(kind: Option<&RegisteredType>) -> Value {
    kind.map(|kind| kind.kind.defaults().config)
        .unwrap_or(Value::Null)
}

/// The type's mark, or the robot.
pub fn type_icon(kind: Option<&RegisteredType>, size: f32, cx: &App) -> AnyElement {
    match kind.and_then(|kind| kind.kind.mark()) {
        Some(mark) => Brand::new(mark).size(size).into_any_element(),
        None => Icon::new(
            kind.and_then(|kind| kind.kind.icon())
                .unwrap_or(IconName::Robot),
        )
        .size(size)
        .color(cx.colors().text_standard)
        .into_any_element(),
    }
}

pub struct CreateAgent {
    name: Entity<InputState>,
    picture: String,
    view: View,
    /// One random avatar of each pack, shown on its collection.
    representatives: Vec<&'static packs::PackAvatar>,
    image: Option<(Entity<AvatarEditor>, [Subscription; 2])>,
    type_key: Option<SharedString>,
    config: Value,
    dirty: bool,
    pending: bool,
    error: Option<SharedString>,
    instructions: Entity<TextareaState>,
    /// While the instructions take over the card: what they were before.
    editing: Option<String>,
    menu_open: bool,
    /// The type whose model submenu shows.
    submenu: Option<SharedString>,
    /// The custom model form, for this type.
    custom: Option<(SharedString, Entity<InputState>)>,
    host: Option<Entity<HostConnection>>,
    host_open: bool,
    /// The chosen type's own form (`Create`), by type key.
    form: Option<(SharedString, AnyView)>,
    example: usize,
    _subscriptions: Vec<Subscription>,
    _examples: Task<()>,
}

impl EventEmitter<CreateEvent> for CreateAgent {}

impl CreateAgent {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Agent name"));
        name.update(cx, |state, cx| state.focus(window, cx));
        let instructions = cx.new(|cx| TextareaState::new(window, cx).placeholder(PLACEHOLDER));
        let subscriptions = vec![
            cx.subscribe_in(
                &name,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.create(window, cx),
                    InputEvent::Change => {
                        this.dirty = true;
                        this.error = None;
                        cx.notify()
                    }
                    _ => {}
                },
            ),
            cx.subscribe(&instructions, |this, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = state.read(cx).value().to_string();
                    this.set_field("instructions", Value::from(value));
                    cx.notify();
                }
            }),
            // A provider's catalog or defaults change its model menu.
            cx.observe_global::<crate::agents2::ProviderChanges>(|_, cx| cx.notify()),
        ];
        let kind = types(cx).into_iter().next();
        let examples = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(EXAMPLE_EVERY).await;
                let alive = this.update(cx, |this, cx| {
                    if !cx.reduce_motion() {
                        this.example = (this.example + 1) % EXAMPLES.len();
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        Self {
            name,
            picture: packs::random().url.clone(),
            view: View::Selected,
            representatives: packs::packs()
                .iter()
                .filter_map(|pack| {
                    let members = packs::of_pack(&pack.id);
                    members.get(packs::random_below(members.len())).copied()
                })
                .collect(),
            image: None,
            config: starting_config(kind.as_ref()),
            type_key: kind.map(|kind| kind.key),
            dirty: false,
            pending: false,
            error: None,
            instructions,
            editing: None,
            menu_open: false,
            submenu: None,
            custom: None,
            host: None,
            host_open: false,
            form: None,
            example: 0,
            _subscriptions: subscriptions,
            _examples: examples,
        }
    }

    /// Makes the chosen type's form, if it has one, once per type.
    fn ensure_form(
        &mut self,
        kind: Option<&RegisteredType>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = kind else {
            return;
        };
        if self.form.as_ref().is_some_and(|(key, _)| *key == kind.key) {
            return;
        }
        let dialog = cx.weak_entity();
        let change: crate::agents2::registry::CreateChange = std::rc::Rc::new(move |config, cx| {
            let _ = dialog.update(cx, |this, cx| {
                this.config = config;
                this.dirty = true;
                cx.notify();
            });
        });
        self.form = kind
            .kind
            .create_form(&self.config, change, window, cx)
            .map(|form| (kind.key.clone(), form));
    }

    fn kind(&self, cx: &App) -> Option<RegisteredType> {
        let all = types(cx);
        self.type_key
            .as_ref()
            .and_then(|key| all.iter().find(|kind| &kind.key == key).cloned())
            .or_else(|| all.into_iter().next())
    }

    fn set_field(&mut self, field: &str, value: Value) {
        if !self.config.is_object() {
            self.config = Value::Object(Default::default());
        }
        if let Value::Object(fields) = &mut self.config {
            fields.insert(field.into(), value);
        }
        self.dirty = true;
    }

    fn instructions(&self) -> String {
        self.config
            .get("instructions")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn choose(&mut self, key: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.submenu = None;
        if self.type_key.as_ref() == Some(&key) {
            return cx.notify();
        }
        let kind = types(cx).into_iter().find(|kind| kind.key == key);
        self.type_key = Some(key);
        self.config = starting_config(kind.as_ref());
        self.form = None;
        self.sync_instructions(window, cx);
        self.dirty = true;
        cx.notify();
    }

    fn choose_model(
        &mut self,
        key: SharedString,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu_open = false;
        self.submenu = None;
        let all = types(cx);
        let Some(kind) = all.iter().find(|kind| kind.key == key) else {
            return;
        };
        let config = if self.type_key.as_ref() == Some(&key) {
            self.config.clone()
        } else {
            starting_config(Some(kind))
        };
        self.config = kind.kind.with_model(config, &model, cx);
        self.type_key = Some(key);
        self.sync_instructions(window, cx);
        self.dirty = true;
        cx.notify();
    }

    /// Shows the config's instructions in the editor.
    fn sync_instructions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.instructions();
        self.instructions
            .update(cx, |state, cx| state.set_value(value, window, cx));
    }

    fn open_custom(&mut self, key: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.submenu = None;
        let current = if self.type_key.as_ref() == Some(&key) {
            self.config
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        } else {
            String::new()
        };
        let listed = types(cx)
            .into_iter()
            .find(|kind| kind.key == key)
            .and_then(|kind| kind.kind.create_models(cx))
            .is_some_and(|models| {
                current.is_empty() || models.models.iter().any(|model| model.id == current)
            });
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Enter a model ID")
                .default_value(if listed { String::new() } else { current })
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.custom = Some((key, input));
        cx.notify();
    }

    fn use_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, input)) = self.custom.take() else {
            return;
        };
        let model = input.read(cx).value().trim().to_owned();
        if model.is_empty() {
            self.custom = Some((key, input));
            return;
        }
        self.choose_model(key, model, window, cx);
    }

    fn choose_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let community = crate::community::AppState::current(cx);
        let current = if packs::by_url(&self.picture).is_some() {
            String::new()
        } else {
            self.picture.clone()
        };
        let editor = cx.new(|cx| AvatarEditor::new(current, community, window, cx));
        let subscriptions = [
            cx.subscribe(&editor, |this, _, AvatarDone(picture), cx| {
                // Done with no image keeps the avatar it had.
                if !picture.is_empty() {
                    this.picture = picture.clone();
                    this.dirty = true;
                }
                this.image = None;
                this.view = View::Selected;
                cx.notify();
            }),
            cx.subscribe(&editor, |this, _, _: &AvatarDismissed, cx| {
                this.image = None;
                this.view = View::Selected;
                cx.notify();
            }),
        ];
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        self.image = Some((editor, subscriptions));
        self.view = View::Image;
        cx.notify();
    }

    /// Back from a sub-view or the instructions editor; `false` when there
    /// is none.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(before) = self.editing.take() {
            // Back restores what the instructions were when editing began.
            self.set_field("instructions", Value::from(before));
            self.sync_instructions(window, cx);
            cx.notify();
            return true;
        }
        let next = match &self.view {
            View::Selected => return false,
            View::Collections => View::Selected,
            View::Pack(_) | View::Image => View::Collections,
        };
        self.image = None;
        self.view = next;
        cx.notify();
        true
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.pending {
            cx.emit(CreateEvent::Close);
        }
    }

    fn create(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() || self.view != View::Selected {
            return;
        }
        let name = self.name.read(cx).value().trim().to_string();
        let (Some(kind), Some(service)) = (self.kind(cx), Agents2::global(cx)) else {
            return;
        };
        if name.is_empty() || self.pending || self.image.is_some() {
            return;
        }
        self.pending = true;
        self.error = None;
        let made = Made {
            config: (!self.config.is_null()).then(|| self.config.clone()),
            picture: (!self.picture.is_empty()).then(|| self.picture.clone()),
            carry: None,
        };
        let host = self.host.clone();
        // A hosted type's provider makes and runs it (`remote`).
        let task: Task<Result<String, String>> = if crate::agents2::is_hosted(&kind.key, cx) {
            let picture = made.picture;
            service.update(cx, |service, cx| {
                service.create_hosted(&kind.key, &name, picture, cx)
            })
        } else {
            let task = service.update(cx, |service, cx| {
                service.create(&kind.key, &name, made, host, cx)
            });
            cx.background_spawn(async move { task.await.map(|agent| agent.pubkey) })
        };
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(pubkey) => cx.emit(CreateEvent::Created(pubkey)),
                    Err(error) => {
                        this.error = Some(error.into());
                        this.pending = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ----- the card -----

    fn artwork(avatar: &packs::PackAvatar) -> Img {
        img(avatar.asset())
            .size_full()
            .object_fit(ObjectFit::Contain)
    }

    fn selected_view(&self, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let name = self.name.read(cx).value().to_string();
        let selected: AnyElement = match packs::by_url(&self.picture) {
            Some(avatar) => Self::artwork(avatar).into_any_element(),
            None => {
                let relay = crate::community::AppState::current(cx)
                    .map(|community| community.read(cx).origin.http().to_owned())
                    .unwrap_or_default();
                match card::picture_source(&self.picture, &relay, cx) {
                    Some(source) => img(source)
                        .size_full()
                        .rounded(radius::CONTAINER)
                        .object_fit(ObjectFit::Cover)
                        .into_any_element(),
                    None => {
                        let initials: String = name.trim().chars().take(2).collect();
                        div()
                            .text_heading()
                            .child(if initials.is_empty() {
                                "?".to_string()
                            } else {
                                initials
                            })
                            .into_any_element()
                    }
                }
            }
        };
        let editing = self.editing.is_some();
        let pending = self.pending;
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(rems(1.))
            .pt(rems(if editing { 0.5 } else { 2. }))
            .child(
                div()
                    .id("avatar-choice")
                    .group("avatar-choice")
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(rems(if editing { 8.5 } else { 13. }))
                    .child(selected)
                    .when(!editing, |this| {
                        this.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .invisible()
                                .group_hover("avatar-choice", |this| this.visible())
                                .child(
                                    Button::new("choose-avatar", "Choose avatar")
                                        .prominent()
                                        .disabled(pending)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.view = View::Collections;
                                            cx.notify();
                                        })),
                                ),
                        )
                    }),
            )
            .when(!(editing && name.trim().is_empty()), |this| {
                this.child(
                    div().w_full().px(rems(3.)).child(
                        TextField::new(&self.name)
                            .surface(Surface::Floating)
                            .disabled(pending || editing),
                    ),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .w_full()
                    .px(rems(3.))
                    .pb(rems(2.))
                    .child(self.details(cx)),
            )
            .text_color(c.text_standard)
    }

    /// The type's part of the card.
    fn details(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let Some(kind) = self.kind(cx) else {
            return div().into_any_element();
        };
        if let Some((key, form)) = &self.form
            && *key == kind.key
        {
            return form.clone().into_any_element();
        }
        if !kind.kind.create_instructions() {
            return div()
                .text_center()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("Set it up after it is made.")
                .into_any_element();
        }
        if self.editing.is_some() {
            return TextArea::new(&self.instructions)
                .rows(9)
                .surface(Surface::Floating)
                .disabled(self.pending)
                .into_any_element();
        }
        let value = self.instructions();
        let has = !value.trim().is_empty();
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(rems(0.75))
            .child(
                div()
                    .max_w_full()
                    .text_center()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .line_clamp(2)
                    .child(if has {
                        value.clone()
                    } else {
                        EXAMPLES[self.example].to_string()
                    }),
            )
            .child(
                Button::new(
                    "agent-instructions",
                    if has {
                        "Edit instructions"
                    } else {
                        "Add instructions"
                    },
                )
                .prominent()
                .disabled(self.pending)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.editing = Some(this.instructions());
                    this.instructions
                        .update(cx, |state, cx| state.focus(window, cx));
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    fn collections_view(&self, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let selected = packs::by_url(&self.picture);
        let tile = |id: ElementId| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(rems(0.75))
                .min_h(rems(13.))
                .p(rems(0.75))
                .border_1()
                .border_color(c.border_standard)
                .rounded(radius::CONTAINER)
                .bg(c.affordance_subtle)
                .cursor_pointer()
                .hover(|this| this.bg(c.affordance_subtle_hover))
        };
        let packs_row = packs::packs()
            .iter()
            .zip(&self.representatives)
            .map(|(pack, avatar)| {
                let shown = match selected {
                    Some(selected) if selected.collection_id == pack.id => selected,
                    _ => avatar,
                };
                let id = pack.id.clone();
                tile(ElementId::Name(format!("pack-{}", pack.id).into()))
                    .child(div().h(rems(9.)).w_full().child(Self::artwork(shown)))
                    .child(pack.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.pending {
                            this.view = View::Pack(id.clone());
                            cx.notify();
                        }
                    }))
            });
        div()
            .grid()
            .grid_cols(4)
            .gap(rems(0.75))
            .w_full()
            .p(rems(1.5))
            .children(packs_row)
            .child(
                tile("use-image".into())
                    .child(
                        Icon::new(IconName::CloudUpload)
                            .size(48.)
                            .color(c.text_standard),
                    )
                    .child("Use image")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.pending {
                            this.choose_image(window, cx)
                        }
                    })),
            )
    }

    fn pack_view(&self, pack: &str, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let choices = packs::of_pack(pack);
        let (columns, size) = pack_grid(choices.len(), PACK_AREA.0, PACK_AREA.1);
        let options = choices.into_iter().map(|avatar| {
            let url = avatar.url.clone();
            let pressed = self.picture == avatar.url;
            div()
                .id(ElementId::Name(format!("pack-avatar-{}", avatar.id).into()))
                .flex()
                .items_center()
                .justify_center()
                .h(px(size))
                .p(rems(0.25))
                .border_1()
                .border_color(if pressed {
                    c.border_focus
                } else {
                    gpui_kit::transparent_black()
                })
                .rounded(radius::CONTROL)
                .bg(c.affordance_subtle_hover)
                .cursor_pointer()
                .tooltip(buzz_ui::Tooltip::text(avatar.label.clone()))
                .child(Self::artwork(avatar))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.pending {
                        this.picture = url.clone();
                        this.dirty = true;
                        this.view = View::Selected;
                        cx.notify();
                    }
                }))
        });
        div()
            .grid()
            .grid_cols(columns as u16)
            .gap(px(8.))
            .w_full()
            .py(rems(1.))
            .px(rems(3.))
            .children(options)
    }

    // ----- the footer -----

    /// The type and model selector, and its menu while open.
    fn selector(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let all = types(cx);
        let kind = self.kind(cx);
        let model_label = kind
            .as_ref()
            .and_then(|kind| kind.kind.create_model_label(&self.config, cx));
        let title = kind
            .as_ref()
            .map(|kind| kind.kind.title().to_string())
            .unwrap_or_default();
        let label = match model_label {
            Some(model) => format!("{title} ({model})"),
            None => title.clone(),
        };
        let menu_offered = all.len() > 1
            || kind
                .as_ref()
                .is_some_and(|kind| kind.kind.create_models(cx).is_some());
        if !menu_offered {
            return div()
                .flex()
                .items_center()
                .gap(rems(0.25))
                .px(rems(0.5))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(type_icon(kind.as_ref(), 16., cx))
                .child(title)
                .into_any_element();
        }
        let pending = self.pending;
        let trigger = div()
            .id("agent-type-selector")
            .flex()
            .items_center()
            .gap(rems(0.375))
            .h(rems(2.))
            .px(rems(0.625))
            .rounded(radius::CONTROL)
            .text_label_sm()
            .when(!pending, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(c.affordance_subtle_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.menu_open = !this.menu_open;
                        this.submenu = None;
                        cx.notify();
                    }))
            })
            .when(pending, |this| this.opacity(0.5))
            .child(type_icon(kind.as_ref(), 16., cx))
            .child(label)
            .child(
                Icon::new(IconName::CaretDown)
                    .size(14.)
                    .color(c.text_subtle),
            );
        let menu = self.menu_open.then(|| {
            let mut menu = Menu::new();
            for (index, item) in all.iter().enumerate() {
                let checked = kind.as_ref().map(|k| &k.key) == Some(&item.key);
                let key = item.key.clone();
                let has_models = item.kind.create_models(cx).is_some();
                let row = MenuItem::new(("agent-type", index), item.kind.title())
                    .leading(type_icon(Some(item), 16., cx))
                    .checked(checked)
                    .when(has_models, |this| {
                        this.submenu()
                            .highlighted(self.submenu.as_ref() == Some(&item.key))
                    });
                let row =
                    if has_models {
                        let key = key.clone();
                        row.on_click(cx.listener(move |this, _, _, cx| {
                            this.submenu = Some(key.clone());
                            cx.notify();
                        }))
                    } else {
                        row.on_click(cx.listener(move |this, _, window, cx| {
                            this.choose(key.clone(), window, cx)
                        }))
                    };
                let hover_key = item.key.clone();
                menu = menu.child(
                    div()
                        .id(("agent-type-row", index))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered {
                                this.submenu = has_models.then(|| hover_key.clone());
                                cx.notify();
                            }
                        }))
                        .child(row),
                );
            }
            let submenu = self.submenu.as_ref().and_then(|key| {
                let item = all.iter().find(|item| &item.key == key)?;
                let models = item.kind.create_models(cx)?;
                let active = kind.as_ref().map(|k| &k.key) == Some(key);
                let value = if active {
                    self.config
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    String::new()
                };
                Some(self.model_menu(key.clone(), &models, active.then_some(value), cx))
            });
            div()
                .flex()
                .items_start()
                .gap(px(4.))
                .child(menu)
                .children(submenu)
        });
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.menu_open = false;
                this.submenu = None;
                cx.notify();
            });
        });
        div()
            .relative()
            .child(trigger)
            .children(menu.map(|menu| overlay::dropup(window, Align::Start, menu, dismiss)))
            .into_any_element()
    }

    /// A type's model rows (`ModelPicker` as a menu). `value` is the chosen
    /// model while this type is the chosen one.
    fn model_menu(
        &self,
        key: SharedString,
        models: &CreateModels,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<(String, String, Option<String>)> = vec![(
            String::new(),
            models.default_label.clone(),
            models.default_description.clone(),
        )];
        rows.extend(
            models
                .models
                .iter()
                .map(|m| (m.id.clone(), m.label.clone(), m.description.clone())),
        );
        if let Some(value) = value.as_ref().filter(|value| !value.is_empty())
            && !rows.iter().any(|(id, ..)| id == value)
        {
            rows.push((
                value.clone(),
                value.clone(),
                Some(if models.allow_custom {
                    "Custom model".into()
                } else {
                    "Unavailable model".into()
                }),
            ));
        }
        let disabled = self.pending || models.loading;
        let mut menu = Menu::new().size(buzz_ui::MenuSize::Wide);
        for (index, (id, label, description)) in rows.into_iter().enumerate() {
            let (key, model) = (key.clone(), id.clone());
            let checked = value.as_deref() == Some(id.as_str());
            let icon = super::model_icon(&format!("{id} {label}"), 18., cx);
            menu = menu.child(
                MenuItem::new(("agent-model", index), label)
                    .leading(icon)
                    .when_some(description, |this, description| {
                        this.trailing(
                            div()
                                .text_caption()
                                .text_color(cx.colors().text_subtle)
                                .child(description),
                        )
                    })
                    .checked(checked)
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose_model(key.clone(), model.clone(), window, cx)
                    })),
            );
        }
        if models.allow_custom {
            let key = key.clone();
            menu = menu.child(
                MenuItem::new("agent-model-custom", "Custom model")
                    .leading(
                        Icon::new(IconName::Robot)
                            .size(18.)
                            .color(cx.colors().text_standard),
                    )
                    .trailing(
                        div()
                            .text_caption()
                            .text_color(cx.colors().text_subtle)
                            .child("Enter a model ID"),
                    )
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_custom(key.clone(), window, cx)
                    })),
            );
        }
        menu.into_any_element()
    }

    /// The Host select, when more than one host can hold the key.
    fn host_select(
        &self,
        hosts: &[Entity<HostConnection>],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let selected = self
            .host
            .clone()
            .or_else(|| {
                hosts
                    .iter()
                    .find(|host| host.read(cx).remote.is_none())
                    .cloned()
            })
            .or_else(|| hosts.first().cloned());
        let label = selected.as_ref().map(|host| host.read(cx).label.clone());
        let mut menu = Menu::new();
        for (index, host) in hosts.iter().enumerate() {
            let checked = selected.as_ref().map(Entity::entity_id) == Some(host.entity_id());
            let online = matches!(host.read(cx).state, ConnectionState::Connected { .. });
            let pick = host.clone();
            menu = menu.child(
                MenuItem::new(("agents2-host", index), host.read(cx).label.clone())
                    .checked(checked)
                    .disabled(!online)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.host = Some(pick.clone());
                        this.host_open = false;
                        cx.notify();
                    })),
            );
        }
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.host_open = false;
                cx.notify();
            });
        });
        let select = Select::new("agents2-host-select")
            .placeholder("Choose where it runs")
            .when_some(label, |this, label| this.value(label))
            .surface(Surface::Floating)
            .disabled(self.pending)
            .on_click(cx.listener(|this, _, _, cx| {
                this.host_open = !this.host_open;
                cx.notify();
            }));
        overlay::select_menu(
            select,
            self.host_open,
            Align::Start,
            Some(rems(16.)),
            || menu.into_any_element(),
            dismiss,
            window,
        )
    }

    fn custom_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, input) = self.custom.as_ref()?;
        let empty = input.read(cx).value().trim().is_empty();
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.custom = None;
                cx.notify();
            });
        });
        let dialog = Dialog::new("custom-model", "Custom model")
            .compact_header_gap()
            .on_close(cx.listener(|this, _, _, cx| {
                this.custom = None;
                cx.notify();
            }))
            .child(TextField::new(input).surface(Surface::Floating))
            .footer(
                Button::new("custom-model-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.custom = None;
                        cx.notify();
                    })),
            )
            .footer(
                Button::new("custom-model-use", "Use model")
                    .prominent()
                    .disabled(empty)
                    .on_click(cx.listener(|this, _, window, cx| this.use_custom(window, cx))),
            );
        Some(overlay::modal(window, dialog, Some(dismiss)).into_any_element())
    }
}

impl Render for CreateAgent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let kind = self.kind(cx);
        self.ensure_form(kind.as_ref(), window, cx);
        // The form follows the config and whether the agent is being made.
        if let Some((_, form)) = &self.form {
            crate::external_plugins::ExternalView::agent_create_changed(
                form,
                &self.config,
                self.pending,
                cx,
            );
        }
        let name_empty = self.name.read(cx).value().trim().is_empty();
        let hosts = Agents2::global(cx)
            .map(|service| service.read(cx).hosts(cx))
            .unwrap_or_default();
        let sub_view = self.view != View::Selected;
        let back = self.editing.is_some() || sub_view;
        let body: AnyElement = match self.view.clone() {
            View::Selected => self.selected_view(cx).size_full().into_any_element(),
            View::Collections => self.collections_view(cx).into_any_element(),
            View::Pack(pack) => self.pack_view(&pack, cx).into_any_element(),
            View::Image => div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .children(
                    self.image
                        .as_ref()
                        .map(|(editor, _)| div().w(rems(22.)).child(editor.clone())),
                )
                .into_any_element(),
        };
        let identity = div()
            .id("agent-identity")
            .flex()
            .flex_col()
            .flex_none()
            .h(rems(32.5))
            .min_w_0()
            .overflow_hidden()
            .border_1()
            .border_color(c.border_standard)
            .rounded(radius::CONTAINER)
            .bg(c.surface_panel)
            .child(body);
        let leading: AnyElement = if back {
            Button::new("create-back", "Back")
                .surface(Surface::Floating)
                .disabled(self.pending)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.back(window, cx);
                }))
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(rems(0.5))
                .child(self.selector(window, cx))
                .when(hosts.len() > 1, |this| {
                    this.child(
                        div()
                            .w(rems(12.))
                            .child(self.host_select(&hosts, window, cx)),
                    )
                })
                .into_any_element()
        };
        let editing = self.editing.is_some();
        let primary = (!sub_view).then(|| {
            if editing {
                Button::new("create-done-editing", "Done editing")
                    .prominent()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.editing = None;
                        cx.notify();
                    }))
            } else {
                Button::new("create-agent", "Create agent")
                    .prominent()
                    .loading(self.pending)
                    .disabled(self.image.is_some() || kind.is_none() || name_empty || self.pending)
                    .on_click(cx.listener(|this, _, window, cx| this.create(window, cx)))
            }
        });
        let dialog = Dialog::new("create-agent-dialog", "Create agent")
            .size(DialogSize::Wide)
            .on_close(cx.listener(|this, _, _, cx| this.close(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.75))
                    .key_context("AgentsCreate")
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            if !this.back(window, cx) && !this.dirty && !this.pending {
                                this.close(cx);
                            }
                        }
                    }))
                    .child(identity)
                    .children(problem(self.error.as_ref(), &c)),
            )
            .footer_leading(leading)
            .when_some(primary, |dialog, primary| dialog.footer(primary));
        let weak = cx.entity().downgrade();
        // A started draft only closes from the close button.
        let dismiss: Option<Dismiss> = (!self.dirty && !self.pending).then(|| {
            Rc::new(move |_: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |this, cx| this.close(cx));
            }) as Dismiss
        });
        div()
            .child(overlay::modal(window, dialog, dismiss))
            .children(self.custom_dialog(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn pack_avatars_take_the_largest_tiles_that_fit() {
        assert_eq!(pack_grid(19, 752., 488.).0, 5);
        let (columns, size) = pack_grid(7, 752., 488.);
        assert_eq!(columns, 4);
        assert!(size > 180.);
    }
}
