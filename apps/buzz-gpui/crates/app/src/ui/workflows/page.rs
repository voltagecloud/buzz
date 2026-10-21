//! One community's workflows (`WorkflowCommunity` in `WorkflowsPage.tsx`):
//! the channel filter, the landing, the channel view or detail editor, the
//! create flow that starts by choosing a channel, and webhook secrets.

use super::channel::{ChannelEvent, Selection, WorkflowChannel};
use super::controls;
use super::editor::{self, Editor, EditorHost, Props};
use super::landing::{LandingEvent, LandingView};
use super::model::editor::operation_locked;
use super::model::form::new_draft_yaml;
use super::secrets::SecretsView;
use crate::community::Community;
use crate::ui::overlay::{self, Align};
use crate::ui::toasts;
use buzz_relay_client::search::match_name;
use buzz_relay_client::workflows::{WorkflowDefinition, Workflows};
use buzz_relay_client::{ChannelSummary, RosterStatus};
use buzz_ui::{
    ActiveTheme, Button, ButtonVariant, Icon, IconButton, IconButtonSize, IconName, Menu, MenuItem,
    MenuLabel, Surface, TextField, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// What the channel view shows: browsing a channel, or one workflow.
#[derive(Clone, PartialEq)]
enum Opened {
    Browse,
    Detail(Selection),
}

/// The create flow's channel picker (`WorkflowChannelPicker.tsx`).
struct Picker {
    input: Entity<InputState>,
    highlighted: usize,
    _subscription: Subscription,
}

pub struct WorkflowCommunity {
    community: Entity<Community>,
    workflows: Workflows,
    viewer: String,
    channels: Vec<ChannelSummary>,
    selected: Option<String>,
    opened: Opened,
    run: bool,
    filter_open: bool,
    landing: Entity<LandingView>,
    channel: Option<(String, Entity<WorkflowChannel>, Subscription)>,
    secrets: Entity<SecretsView>,
    create: Option<Editor>,
    picker: Option<Picker>,
    deleting: bool,
    _subscriptions: Vec<Subscription>,
}

fn participating(community: &Community) -> Vec<ChannelSummary> {
    community
        .snapshot
        .channels
        .iter()
        .filter(|channel| !channel.cached && !channel.read_only)
        .cloned()
        .collect()
}

impl WorkflowCommunity {
    pub fn new(community: Entity<Community>, cx: &mut Context<Self>) -> Self {
        let (workflows, viewer, channels) = {
            let community = community.read(cx);
            (
                community.session.workflows().clone(),
                community.session.viewer().to_string(),
                participating(community),
            )
        };
        let landing = {
            let workflows = workflows.clone();
            let viewer = viewer.clone();
            let channels = channels.clone();
            cx.new(|cx| LandingView::new(workflows, viewer, channels, cx))
        };
        let secrets = {
            let workflows = workflows.clone();
            cx.new(|cx| SecretsView::new(workflows, cx))
        };
        let subscriptions = vec![
            cx.observe(&community, |this, community, cx| {
                let channels = participating(community.read(cx));
                if channels != this.channels {
                    this.channels = channels.clone();
                    this.landing
                        .update(cx, |landing, _| landing.set_channels(channels));
                    // A channel that is gone closes its view, once the
                    // roster is known.
                    let ready = community.read(cx).snapshot.roster == RosterStatus::Ready;
                    if ready
                        && let Some(selected) = &this.selected
                        && !this.channels.iter().any(|c| &c.id == selected)
                    {
                        this.open_channel(None, Opened::Browse, false, cx);
                    }
                }
                cx.notify();
            }),
            cx.subscribe(&landing, |this, _, event: &LandingEvent, cx| match event {
                LandingEvent::Open(definition, channel, run) => this.open_channel(
                    Some(channel.id.clone()),
                    Opened::Detail(Selection::Definition(definition.clone())),
                    *run,
                    cx,
                ),
                LandingEvent::Delete(definition) => this.delete(definition.clone(), cx),
                LandingEvent::Create => {
                    this.create = Some(Editor::new(&new_draft_yaml(), true, true, cx));
                    this.picker = None;
                    cx.notify();
                }
            }),
        ];
        Self {
            community,
            workflows,
            viewer,
            channels,
            selected: None,
            opened: Opened::Browse,
            run: false,
            filter_open: false,
            landing,
            channel: None,
            secrets,
            create: None,
            picker: None,
            deleting: false,
            _subscriptions: subscriptions,
        }
    }

    /// `openChannel`: the channel view keyed by channel and selection, so
    /// a different workflow starts a fresh editor.
    fn open_channel(
        &mut self,
        channel: Option<String>,
        opened: Opened,
        run: bool,
        cx: &mut Context<Self>,
    ) {
        self.selected = channel.clone();
        self.opened = opened.clone();
        self.run = run;
        self.filter_open = false;
        self.channel = None;
        if let Some(id) = channel
            && let Some(summary) = self.channels.iter().find(|c| c.id == id).cloned()
        {
            let key = match &opened {
                Opened::Browse => format!("{id}:browse"),
                Opened::Detail(Selection::New) => format!("{id}:new"),
                Opened::Detail(Selection::Definition(d)) => format!("{id}:{}", d.revision),
            };
            let workflows = self.workflows.clone();
            let viewer = self.viewer.clone();
            let initial = match opened {
                Opened::Browse => None,
                Opened::Detail(selection) => Some(selection),
            };
            let view = cx.new(|cx| {
                WorkflowChannel::new(
                    workflows,
                    summary.id,
                    summary.name,
                    viewer,
                    initial,
                    run,
                    cx,
                )
            });
            let subscription =
                cx.subscribe(&view, |this, _, event: &ChannelEvent, cx| match event {
                    ChannelEvent::Close => this.open_channel(None, Opened::Browse, false, cx),
                    ChannelEvent::Delete(definition) => this.delete(definition.clone(), cx),
                    ChannelEvent::SaveReadback { channel, revision } => {
                        let (channel, revision) = (channel.clone(), revision.clone());
                        this.landing
                            .update(cx, |landing, _| landing.readback(&channel, &revision));
                    }
                });
            self.channel = Some((key, view, subscription));
        }
        cx.notify();
    }

    /// `deleteWorkflow`: submits and discards the editor draft, whatever
    /// happens; the landing owns readback and recovery.
    fn delete(&mut self, definition: WorkflowDefinition, cx: &mut Context<Self>) {
        if self.deleting {
            return;
        }
        if operation_locked(&self.workflows.operations(), &definition) {
            self.deletion_error(
                "Another workflow change is still being confirmed. Refresh and try again.".into(),
                cx,
            );
            self.open_channel(None, Opened::Browse, false, cx);
            return;
        }
        if definition.owner != self.viewer || !self.workflows.availability().delete {
            return;
        }
        self.deleting = true;
        match self.workflows.delete(&definition) {
            Ok(_) => toasts::dismiss("workflow-delete-error", cx),
            Err(error) => self.deletion_error(error, cx),
        }
        self.deleting = false;
        // Confirmation deliberately discards the editor draft.
        self.open_channel(None, Opened::Browse, false, cx);
    }

    fn deletion_error(&self, error: String, cx: &mut App) {
        toasts::show(
            toasts::Notice::error("workflow-delete-error", "Couldn't start deletion")
                .description(error),
            cx,
        );
    }

    fn picker(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(picker) = &self.picker {
            return picker.input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search channels…"));
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => {
                        if let Some(picker) = &mut this.picker {
                            picker.highlighted = 0;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        let matches = this.matches(cx);
                        let index = this.picker.as_ref().map_or(0, |p| p.highlighted);
                        if let Some(channel) = matches.get(index) {
                            let id = channel.id.clone();
                            this.pick(id, cx);
                        }
                    }
                    _ => {}
                },
            );
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.picker = Some(Picker {
            input: input.clone(),
            highlighted: 0,
            _subscription: subscription,
        });
        input
    }

    /// Channels matching the typed text, best first.
    fn matches(&self, cx: &App) -> Vec<ChannelSummary> {
        let typed = self
            .picker
            .as_ref()
            .map(|p| p.input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();
        if typed.is_empty() {
            return self.channels.clone();
        }
        let mut ranked: Vec<(u8, ChannelSummary)> = self
            .channels
            .iter()
            .filter_map(|channel| Some((match_name(&channel.name, &typed)?.rank, channel.clone())))
            .collect();
        ranked.sort_by_key(|(rank, _)| *rank);
        ranked.into_iter().map(|(_, channel)| channel).collect()
    }

    fn pick(&mut self, id: String, cx: &mut Context<Self>) {
        self.create = None;
        self.picker = None;
        self.open_channel(Some(id), Opened::Detail(Selection::New), false, cx);
    }

    fn picker_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let input = self.picker(window, cx);
        let matches = self.matches(cx);
        let highlighted = self.picker.as_ref().map_or(0, |p| p.highlighted);
        let mut list = div()
            .id("workflow-channel-options")
            .flex()
            .flex_col()
            .max_h(rems(20.))
            .overflow_y_scroll()
            .p(space::S1);
        if matches.is_empty() {
            list = list.child(
                div()
                    .px(space::S2)
                    .py(space::S2)
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child("No matching channels."),
            );
        }
        let ix = c.interaction(Surface::Floating);
        for (index, channel) in matches.iter().enumerate() {
            let id = channel.id.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("workflow-pick-{}", channel.id)))
                    .flex()
                    .items_center()
                    .gap(space::S2)
                    .min_h(rems(2.))
                    .px(space::S2)
                    .rounded(radius::ROW)
                    .cursor_pointer()
                    .when(index == highlighted, |this| this.bg(ix.row_hover))
                    .hover(move |s| s.bg(ix.row_hover))
                    .child(
                        Icon::new(if channel.private {
                            IconName::Lock
                        } else {
                            IconName::Hash
                        })
                        .size(18.),
                    )
                    .child(div().truncate().child(channel.name.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(id.clone(), cx))),
            );
        }
        div()
            .w_full()
            .max_w(rems(25.))
            .mx_auto()
            .my(space::S6)
            .flex()
            .flex_col()
            .gap(space::S2)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let count = this.matches(cx).len();
                if let Some(picker) = &mut this.picker
                    && count > 0
                {
                    match event.keystroke.key.as_str() {
                        "down" => picker.highlighted = (picker.highlighted + 1) % count,
                        "up" => picker.highlighted = (picker.highlighted + count - 1) % count,
                        _ => return,
                    }
                    cx.notify();
                }
            }))
            .child(controls::field(
                "Choose a channel",
                None,
                None,
                TextField::new(&input)
                    .surface(Surface::Floating)
                    .trailing(Icon::new(IconName::CaretDown).size(16.)),
                cx,
            ))
            .child(
                div()
                    .bg(c.surface_popover)
                    .border_1()
                    .border_color(c.border_standard)
                    .rounded(radius::CONTAINER)
                    .child(list),
            )
            .into_any_element()
    }

    fn header(&self, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let browsing = self.selected.is_some() && self.opened == Opened::Browse;
        div()
            .flex()
            .items_start()
            .justify_between()
            .gap(space::S4)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_title().child("Workflows"))
                    .child(
                        div()
                            .mt(space::S2)
                            .text_body()
                            .text_color(c.text_subtle)
                            .child("Automations that keep your community moving."),
                    ),
            )
            .child(div().flex_none().child(if browsing {
                Button::new("workflows-all", "All workflows")
                    .ghost()
                    .icon(IconName::ArrowLeft)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_channel(None, Opened::Browse, false, cx)
                    }))
                    .into_any_element()
            } else {
                IconButton::new("workflows-refresh", IconName::ArrowsClockwise)
                    .size(IconButtonSize::Md)
                    .tooltip("Refresh workflows")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.landing.update(cx, |landing, _| landing.refresh());
                    }))
                    .into_any_element()
            }))
    }

    fn filter(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.channels.iter().find(|c| &c.id == id))
            .map_or_else(|| "All workflows".to_string(), |c| c.name.clone());
        let mut menu = Menu::new()
            .child(MenuLabel::new("Community channels"))
            .child(
                MenuItem::new("filter-all", "All workflows")
                    .checked(self.selected.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.selected.is_some() {
                            this.open_channel(None, Opened::Browse, false, cx);
                        } else {
                            this.filter_open = false;
                            cx.notify();
                        }
                    })),
            );
        let mut list = div()
            .id("filter-channels")
            .flex()
            .flex_col()
            .max_h(rems(18.))
            .overflow_y_scroll();
        for channel in &self.channels {
            let id = channel.id.clone();
            list = list.child(
                MenuItem::new(
                    SharedString::from(format!("filter-{}", channel.id)),
                    channel.name.clone(),
                )
                .checked(self.selected.as_deref() == Some(&channel.id))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.selected.as_deref() != Some(&id) {
                        this.open_channel(Some(id.clone()), Opened::Browse, false, cx);
                    } else {
                        this.filter_open = false;
                        cx.notify();
                    }
                })),
            );
        }
        menu = menu.child(list);
        let entity = cx.entity().downgrade();
        div()
            .flex()
            .items_center()
            .gap(space::S4)
            .my(space::S6)
            .child(
                div()
                    .text_body()
                    .text_color(c.text_standard)
                    .child("Channel"),
            )
            .child(
                div()
                    .relative()
                    .child(
                        Button::new("workflows-filter", current)
                            .variant(ButtonVariant::Ghost)
                            .icon_end(IconName::CaretDown)
                            .selected(self.filter_open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.filter_open = !this.filter_open;
                                cx.notify();
                            })),
                    )
                    .when(self.filter_open, |this| {
                        this.child(overlay::dropdown(
                            window,
                            Align::Start,
                            menu,
                            Rc::new(move |_, cx| {
                                let _ = entity.update(cx, |this, cx| {
                                    this.filter_open = false;
                                    cx.notify();
                                });
                            }),
                        ))
                    }),
            )
    }
}

/// The create flow's editor only chooses a channel; its draft is fixed.
impl EditorHost for WorkflowCommunity {
    fn editor(&mut self) -> Option<&mut Editor> {
        self.create.as_mut()
    }

    fn editor_ref(&self) -> Option<&Editor> {
        self.create.as_ref()
    }

    fn draft_yaml(&self) -> String {
        new_draft_yaml()
    }

    fn draft_changed(&mut self, _: String, _: &mut Window, _: &mut Context<Self>) {}

    fn save_draft(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn cancel_draft(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.create = None;
        self.picker = None;
        cx.notify();
    }
}

impl Render for WorkflowCommunity {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let snapshot = self.community.read(cx).snapshot.clone();
        let mut content = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(rems(72.))
            .mx_auto()
            .child(self.header(cx));
        if snapshot.roster == RosterStatus::Loading && self.channels.is_empty() {
            content = content.child(div().mt(space::S4).child("Reading channels…"));
        }
        if let RosterStatus::Error(error) = &snapshot.roster {
            let error = if error.is_empty() {
                "Channels could not be read.".to_string()
            } else {
                error.clone()
            };
            let community = self.community.clone();
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S2)
                    .py(space::S8)
                    .child(controls::danger(error, cx))
                    .child(
                        div().flex().child(
                            Button::new("workflows-retry-channels", "Retry channels")
                                .on_click(move |_, _, cx| community.read(cx).session.retry_live()),
                        ),
                    ),
            );
        }
        let detail = self.opened != Opened::Browse;
        if !self.channels.is_empty() && !detail {
            content = content.child(self.filter(window, cx));
        }
        if snapshot.roster == RosterStatus::Ready && self.channels.is_empty() {
            content = content.child(
                div()
                    .py(space::S8)
                    .child("No channels are available in this community."),
            );
        }
        let browsing = self.selected.is_some() && !detail;
        if !self.channels.is_empty() && !browsing {
            content = content.child(self.landing.clone());
        }
        if let Some((_, view, _)) = &self.channel {
            content = content.child(view.clone());
        }
        let create = if self.create.is_some() {
            let picker = self.picker_view(window, cx);
            let props = Props {
                read_only: false,
                blocked: None,
                busy: false,
                locked: false,
                create: true,
                choose_channel: true,
                initial_yaml: None,
                scope: Some(picker),
                actions: None,
                status: Vec::new(),
                details: Vec::new(),
                children: Vec::new(),
            };
            Some(editor::render(self, props, window, cx))
        } else {
            None
        };
        div()
            .id("workflows-page")
            .size_full()
            .overflow_y_scroll()
            .p(space::S6)
            .text_body()
            .text_color(c.text_standard)
            .child(content)
            .child(self.secrets.clone())
            .children(create)
    }
}
