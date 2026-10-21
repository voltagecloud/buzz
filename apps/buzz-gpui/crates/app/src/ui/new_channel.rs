//! "Create a channel" (`CreateChannelDialog.tsx`): name, description,
//! template, duration and privacy, with the privacy confirmation and "Discard
//! changes?" as steps of the same dialog. The relay creates the channel
//! with an id chosen here; it opens once the viewer is confirmed as its
//! owner.

use super::form_fields::{self, Bounded, TextKind};
use super::modal::{Dismissed, OpenChannel, error};
use super::overlay::{self, Align, Dismiss};
use super::settings::form;
use crate::agents2::Agents2;
use crate::agents2::teams::Team;
use crate::channel_templates::{ChannelTemplate, ChannelTemplates, template_agents};
use crate::community::AppState;
use crate::community::Community;
use crate::ui::toasts;
use buzz_relay_client::kit::setup::{ChannelCreationInput, ChannelSetup, CreationVisibility};
use buzz_relay_client::lifecycle::{
    DEFAULT_TEMPORARY_TTL, DraftErrors, NewChannel, canonical_name, draft_errors,
};
use buzz_relay_client::sidebar_store::PreferencesStatus;
use buzz_ui::{
    ActiveTheme, Button, Dialog, Menu, MenuItem, Select, Surface, Switch, ToastTone, TypeStyled,
    tokens::space,
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::*;
use std::collections::HashSet;
use std::rc::Rc;

pub struct NewChannelDialog {
    community: Entity<Community>,
    name: Entity<InputState>,
    description: Entity<TextareaState>,
    private: bool,
    temporary: bool,
    /// The temporary duration, kept while Ongoing is chosen.
    temporary_ttl: u64,
    /// The visibility awaiting "Make channel private/public?".
    privacy_choice: Option<bool>,
    skip_warning: bool,
    confirm_discard: bool,
    busy: bool,
    error: Option<SharedString>,
    on_open: OpenChannel,
    /// The sidebar group a new channel goes into (`initialGroup`).
    group: Option<String>,
    /// A creation awaiting "Retry channel"; the form shows it frozen.
    pending: Option<ChannelCreationInput>,
    /// The chosen channel template's id; empty for none.
    template: String,
    template_menu: bool,
    /// Holds focus on the privacy and discard steps, so Escape steps back.
    step_focus: FocusHandle,
    _bounds: [Bounded; 2],
    _subscriptions: Vec<Subscription>,
}

/// What a chosen template sends as channel setup.
struct TemplateSetup {
    id: String,
    /// The owner's agents here among its agents and its teams' members.
    agents: Vec<String>,
    names: Vec<String>,
    canvas: String,
}

impl TemplateSetup {
    /// `mine` names the owner's agents in this community.
    fn new(template: ChannelTemplate, mine: &[(String, String)], teams: &[Team]) -> Self {
        let name = |pubkey: &str| {
            mine.iter()
                .find(|(key, _)| key == pubkey)
                .map(|(_, name)| name.clone())
        };
        let agents = template_agents(&template, teams, |pubkey| name(pubkey).is_some());
        Self {
            id: template.id,
            names: agents.iter().filter_map(|pubkey| name(pubkey)).collect(),
            agents,
            canvas: template.canvas,
        }
    }
}

/// The setup a new channel sends: its sidebar group and what its template
/// adds. A template that would add nothing makes a plain channel.
fn channel_setup(group_id: String, template: Option<TemplateSetup>) -> Option<ChannelSetup> {
    let setup = match template {
        Some(template) => ChannelSetup {
            agents: template.agents,
            canvas: template.canvas,
            group_id,
            template_id: template.id,
        },
        None => ChannelSetup {
            group_id,
            ..Default::default()
        },
    };
    (!setup.group_id.is_empty() || !setup.agents.is_empty() || !setup.canvas.is_empty())
        .then_some(setup)
}

/// The line under the Template select: what the template adds here.
fn template_note(names: &[String], canvas: &str) -> String {
    let parts: Vec<String> = [
        if names.is_empty() {
            String::new()
        } else {
            format!("Adds {}", names.join(", "))
        },
        if canvas.trim().is_empty() {
            String::new()
        } else {
            "Starts the Canvas".into()
        },
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect();
    if parts.is_empty() {
        "None of its agents are in this community, and it has no Canvas.".into()
    } else {
        parts.join(" · ")
    }
}

impl EventEmitter<Dismissed> for NewChannelDialog {}

/// The sidebar group whose "Create channel" opened the dialog, set just
/// before the dialog opens and taken by it.
#[derive(Default)]
pub struct CreateIn(pub Option<String>);

impl Global for CreateIn {}

/// What the dialog holds, apart from its inputs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Draft {
    name: String,
    description: String,
    private: bool,
    temporary: bool,
    /// The chosen template, while it still exists.
    template: String,
}

impl Draft {
    fn errors(&self) -> DraftErrors {
        draft_errors(&canonical_name(&self.name), &self.description, None)
    }

    /// Anything typed or chosen asks before it is thrown away.
    fn dirty(&self) -> bool {
        !self.name.is_empty()
            || !self.description.is_empty()
            || self.private
            || self.temporary
            || !self.template.is_empty()
    }
}

impl NewChannelDialog {
    pub fn new(
        community: Entity<Community>,
        on_open: OpenChannel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("release-notes"));
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("What this channel is for")
                .auto_grow(3, 8)
        });
        name.update(cx, |state, cx| state.focus(window, cx));
        let bounds = [
            Bounded::new(&name, TextKind::Name.limit(), window, cx),
            Bounded::new(&description, TextKind::Description.limit(), window, cx),
        ];
        let mut subscriptions = vec![
            cx.subscribe_in(
                &name,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.create(window, cx),
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify()
                    }
                    _ => {}
                },
            ),
            cx.subscribe(&description, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            }),
        ];
        // The template choices and what they add follow both stores.
        if let Some(templates) = ChannelTemplates::global(cx) {
            subscriptions.push(cx.observe(&templates, |_, _, cx| cx.notify()));
        }
        if let Some(agents) = Agents2::global(cx) {
            subscriptions.push(cx.observe(&agents, |_, _, cx| cx.notify()));
        }
        let group = cx
            .try_global::<CreateIn>()
            .and_then(|create| create.0.clone());
        let session = community.read(cx).session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some(pending) = session.pending_creation().await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| this.restore(pending, window, cx));
        })
        .detach();
        cx.set_global(CreateIn(None));
        Self {
            group,
            pending: None,
            template: String::new(),
            template_menu: false,
            community,
            name,
            description,
            private: false,
            temporary: false,
            temporary_ttl: DEFAULT_TEMPORARY_TTL,
            privacy_choice: None,
            skip_warning: false,
            confirm_discard: false,
            busy: false,
            error: None,
            on_open,
            step_focus: cx.focus_handle(),
            _bounds: bounds,
            _subscriptions: subscriptions,
        }
    }

    /// Shows a creation awaiting confirmation, frozen, for "Retry channel".
    fn restore(
        &mut self,
        pending: ChannelCreationInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.name.update(cx, |state, cx| {
            state.set_value(pending.name.clone(), window, cx)
        });
        let description = pending.description.clone().unwrap_or_default();
        self.description
            .update(cx, |state, cx| state.set_value(description, window, cx));
        self.private = pending.visibility == CreationVisibility::Private;
        self.temporary = pending.ttl_seconds.is_some();
        self.temporary_ttl = pending.ttl_seconds.unwrap_or(DEFAULT_TEMPORARY_TTL);
        self.pending = Some(pending);
        self.error = None;
        cx.notify();
    }

    fn draft(&self, cx: &App) -> Draft {
        Draft {
            name: self.name.read(cx).value().to_string(),
            description: self.description.read(cx).value().to_string(),
            private: self.private,
            temporary: self.temporary,
            template: self
                .template_setup(cx)
                .map(|setup| setup.id)
                .unwrap_or_default(),
        }
    }

    /// The chosen template's setup. A template deleted meanwhile is no
    /// choice, and a team deleted since adds nobody.
    fn template_setup(&self, cx: &App) -> Option<TemplateSetup> {
        let templates = ChannelTemplates::global(cx)?;
        let template = templates
            .read(cx)
            .templates()
            .iter()
            .find(|item| item.id == self.template)?
            .clone();
        let agents2 = Agents2::global(cx);
        let agents2 = agents2.as_ref().map(|agents| agents.read(cx));
        let mine: Vec<(String, String)> = agents2
            .map(|agents| agents.agents())
            .unwrap_or_default()
            .iter()
            .map(|agent| (agent.pubkey.clone(), agent.name.clone()))
            .collect();
        let teams = agents2.map(|agents| agents.teams()).unwrap_or_default();
        Some(TemplateSetup::new(template, &mine, teams))
    }

    fn set_template(&mut self, id: String, cx: &mut Context<Self>) {
        self.template_menu = false;
        if self.editing() {
            self.template = id;
            self.error = None;
        }
        cx.notify();
    }

    /// The Template select, when any template exists and the form is not
    /// a frozen retry.
    fn template_field(&self, window: &Window, cx: &mut Context<Self>) -> Option<Div> {
        let templates = ChannelTemplates::global(cx)?.read(cx).templates().to_vec();
        if templates.is_empty() || self.pending.is_some() {
            return None;
        }
        let setup = self.template_setup(cx);
        let chosen = setup
            .as_ref()
            .map(|setup| setup.id.clone())
            .unwrap_or_default();
        let value = templates
            .iter()
            .find(|item| item.id == chosen)
            .map(|item| SharedString::from(item.name.clone()))
            .unwrap_or_else(|| "None".into());
        let mut select = div().relative().w_full().child(
            Select::new("create-channel-template")
                .value(value)
                .open(self.template_menu)
                .surface(Surface::Floating)
                .disabled(!self.editing())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.template_menu = !this.template_menu && this.editing();
                    cx.notify();
                })),
        );
        if self.template_menu {
            let options = std::iter::once((String::new(), "None".to_owned()))
                .chain(templates.into_iter().map(|item| (item.id, item.name)))
                .enumerate()
                .map(|(index, (id, label))| {
                    MenuItem::new(
                        SharedString::from(format!("channel-template-{index}")),
                        label,
                    )
                    .checked(id == chosen)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_template(id.clone(), cx)))
                });
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.template_menu = false;
                    cx.notify();
                });
            });
            select = select.child(overlay::dropdown(
                window,
                Align::Start,
                div().w(px(448.)).child(Menu::new().children(options)),
                dismiss,
            ));
        }
        let note = setup.map(|setup| template_note(&setup.names, &setup.canvas).into());
        Some(form::field(
            "Template",
            note,
            None,
            select,
            form::FieldStyle::default(),
            cx,
        ))
    }

    /// The form takes edits only on its own step and while idle.
    fn editing(&self) -> bool {
        !self.busy
            && self.privacy_choice.is_none()
            && !self.confirm_discard
            && self.pending.is_none()
    }

    /// Focus follows the step: the name field, or the step for Escape.
    fn after_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.privacy_choice.is_some() || self.confirm_discard {
            self.step_focus.focus(window, cx);
        } else {
            self.name.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// Close, Escape and the scrim step back through privacy and discard.
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.privacy_choice.is_some() {
            self.privacy_choice = None;
        } else if self.confirm_discard {
            self.confirm_discard = false;
        } else if self.pending.is_none() && self.draft(cx).dirty() {
            self.confirm_discard = true;
        } else {
            cx.emit(Dismissed);
            return;
        }
        self.after_step(window, cx);
    }

    fn set_private(&mut self, private: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing() {
            return;
        }
        if form_fields::skip_privacy_confirmation(self.community.read(cx), cx) {
            self.private = private;
            self.error = None;
        } else {
            self.skip_warning = false;
            self.privacy_choice = Some(private);
        }
        self.after_step(window, cx);
    }

    fn confirm_privacy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(private) = self.privacy_choice.take() else {
            return;
        };
        if self.busy {
            return;
        }
        if self.skip_warning {
            form_fields::remember_privacy_confirmation(self.community.read(cx), cx);
        }
        self.private = private;
        self.error = None;
        self.after_step(window, cx);
    }

    fn set_duration(&mut self, ttl: Option<u64>, cx: &mut Context<Self>) {
        if !self.editing() {
            return;
        }
        self.temporary = ttl.is_some();
        if let Some(ttl) = ttl {
            self.temporary_ttl = ttl;
        }
        self.error = None;
        cx.notify();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        let retry = self.pending.is_some() && !self.busy;
        if !retry && (draft.errors().any() || !self.editing()) {
            return;
        }
        // Recovery keeps the original frozen request.
        let input = match &self.pending {
            Some(pending) => pending.clone(),
            None => {
                // A channel created from a sidebar group goes into it.
                let group_id = match &self.group {
                    Some(group) => {
                        let sidebar = self.community.read(cx).snapshot.sidebar.clone();
                        let ready = sidebar.status == PreferencesStatus::Ready;
                        let data = sidebar.data.as_ref();
                        if !ready
                            || !data.is_some_and(|d| d.sections.iter().any(|s| s.id == *group))
                        {
                            self.error = Some(
                                if ready {
                                    "This group is no longer available. Close this dialog and \
                                     create from another sidebar group or Channels."
                                } else {
                                    "Wait for saved sidebar groups to load, then try again."
                                }
                                .into(),
                            );
                            cx.notify();
                            return;
                        }
                        group.clone()
                    }
                    None => String::new(),
                };
                let setup = channel_setup(group_id, self.template_setup(cx));
                let channel = NewChannel {
                    name: canonical_name(&draft.name),
                    description: draft.description.trim().to_owned(),
                    private: self.private,
                    ttl: self.temporary.then_some(self.temporary_ttl),
                    labels: Vec::new(),
                };
                ChannelCreationInput::from_new(&channel, setup)
            }
        };
        let session = self.community.read(cx).session.clone();
        self.busy = true;
        self.error = None;
        cx.spawn_in(window, async move |this, cx| {
            let result = session.create_channel_with(input).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(id) => {
                        let channel = this
                            .community
                            .read(cx)
                            .snapshot
                            .channels
                            .iter()
                            .find(|c| c.id == id)
                            .cloned();
                        if let Some(channel) = channel {
                            (this.on_open)(&channel, None, window, cx);
                        }
                        cx.emit(Dismissed);
                    }
                    Err(message) => {
                        this.error = Some(message.into());
                        let session = this.community.read(cx).session.clone();
                        cx.spawn_in(window, async move |this, cx| {
                            let pending = session.pending_creation().await;
                            let _ = this.update_in(cx, |this, window, cx| {
                                // An unconfirmed Create becomes "Retry channel".
                                let error = this.error.take();
                                match pending {
                                    Some(pending) => this.restore(pending, window, cx),
                                    None => this.pending = None,
                                }
                                this.error = error;
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn dialog(&self, window: &Window, cx: &mut Context<Self>) -> Dialog {
        let close = cx.listener(|this, _: &ClickEvent, window, cx| this.request_close(window, cx));
        if self.confirm_discard {
            return Dialog::new("create-channel", "Discard changes?")
                .description("Your channel draft has unsaved changes.")
                .compact_header_gap()
                .on_close(close)
                .footer(
                    Button::new("keep-editing", "Keep editing")
                        .surface(Surface::Floating)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_discard = false;
                            this.after_step(window, cx);
                        })),
                )
                .footer(
                    Button::new("discard-changes", "Discard changes")
                        .destructive()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.busy {
                                cx.emit(Dismissed);
                            }
                        })),
                );
        }
        if let Some(private) = self.privacy_choice {
            let weak = cx.entity().downgrade();
            return Dialog::new("create-channel", form_fields::privacy_title(private))
                .compact_header_gap()
                .compact_footer_gap()
                .on_close(close)
                .child(form_fields::privacy_confirmation(
                    private,
                    self.skip_warning,
                    move |checked, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.skip_warning = checked;
                            cx.notify();
                        });
                    },
                    cx,
                ))
                .footer(
                    Button::new("privacy-cancel", "Cancel")
                        .surface(Surface::Floating)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.privacy_choice = None;
                            this.after_step(window, cx);
                        })),
                )
                .footer(
                    Button::new("privacy-continue", "Continue")
                        .prominent()
                        .disabled(self.busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_privacy(window, cx)),
                        ),
                );
        }
        let draft = self.draft(cx);
        let errors = draft.errors();
        let weak = cx.entity().downgrade();
        let on_duration: form_fields::OnDuration = Rc::new(move |ttl, _, cx| {
            let _ = weak.update(cx, |this, cx| this.set_duration(ttl, cx));
        });
        let weak = cx.entity().downgrade();
        let pending = self.pending.is_some();
        let summary = self
            .pending
            .as_ref()
            .and_then(|p| p.setup.as_ref())
            .map(|setup| setup_summary(setup.agents.len(), cx));
        let title = match &self.group {
            Some(group) => {
                let name = self
                    .community
                    .read(cx)
                    .snapshot
                    .sidebar
                    .data
                    .as_ref()
                    .and_then(|data| data.sections.iter().find(|s| s.id == *group))
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| "saved group".into());
                format!("Create a channel in {name}")
            }
            None => "Create a channel".into(),
        };
        Dialog::new("create-channel", title)
            .compact_header_gap()
            .on_close(close)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S4)
                    .children(pending.then(|| pending_notice(cx)))
                    // A name error waits until there is a name.
                    .child(form_fields::name_field(
                        &self.name,
                        errors.name.filter(|_| !draft.name.is_empty() && !pending),
                        pending,
                        cx,
                    ))
                    .child(form_fields::description_field(
                        &self.description,
                        errors.description.filter(|_| !pending),
                        pending,
                        cx,
                    ))
                    .children(self.template_field(window, cx))
                    .child(form_fields::duration_field(
                        self.temporary,
                        self.temporary_ttl,
                        pending,
                        None,
                        on_duration,
                        cx,
                    ))
                    .children(summary)
                    .children(self.error.clone().map(|message| error(message, cx))),
            )
            .footer_leading(
                Switch::new("create-private", self.private)
                    .label("Private")
                    .disabled(pending)
                    .on_change(move |checked, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.set_private(checked, window, cx));
                    }),
            )
            .footer(
                Button::new(
                    "create-channel-submit",
                    if pending {
                        "Retry channel"
                    } else {
                        "Create channel"
                    },
                )
                .prominent()
                .loading(self.busy)
                .disabled(errors.any() && !pending)
                .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
            )
    }
}

impl Render for NewChannelDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        // The dialog covers the scrim so a press outside the card steps
        // back (`dismissOnOutsideClick`) instead of closing outright.
        div()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.request_close(window, cx)),
            )
            .child(
                div()
                    .track_focus(&self.step_focus)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            this.request_close(window, cx);
                        }
                    }))
                    .child(self.dialog(window, cx)),
            )
    }
}

/// Raises channel setup notices.
pub fn init(cx: &mut App) {
    setup_notices(cx);
}

/// The Retry notice of a creation that may already exist.
fn pending_notice(cx: &App) -> Div {
    crate::ui::settings::form::line(
        "This attempt may already have created a channel. Retry checks it first and, if unconfirmed, resends only the same creation request. It will not create another channel or continue its setup.",
        false,
        cx,
    )
    .text_body()
}

/// What a recovered creation sets up (`recoverySummary`).
fn setup_summary(agents: usize, cx: &App) -> Div {
    let c = cx.colors();
    div()
        .text_body()
        .text_color(c.text_standard)
        .child(match agents {
            0 => "Only you.".to_owned(),
            1 => "1 selected agent.".to_owned(),
            n => format!("{n} selected agents."),
        })
}

/// "Channel setup couldn’t finish" notices (`ChannelSidebar.tsx`), raised
/// as warning toasts for every community. Dismissing one only hides it.
fn setup_notices(cx: &mut App) {
    let watched: std::rc::Rc<std::cell::RefCell<HashSet<EntityId>>> = Default::default();
    let follow = move |cx: &mut App| {
        let Some(state) = cx.try_global::<AppState>() else {
            return;
        };
        for community in state.communities.clone() {
            if !watched.borrow_mut().insert(community.entity_id()) {
                continue;
            }
            let session = community.read(cx).session.clone();
            let mut creation = session.creation();
            let weak = community.downgrade();
            cx.spawn(async move |cx| {
                let mut shown: HashSet<String> = HashSet::new();
                loop {
                    let notices = creation.borrow_and_update().notices.clone();
                    let alive = cx.update(|cx| {
                        if weak.upgrade().is_none() {
                            return false;
                        }
                        for notice in &notices {
                            if !shown.insert(notice.id.clone()) {
                                continue;
                            }
                            let key = SharedString::from(format!("channel-setup:{}", notice.id));
                            let (id, session, close) =
                                (notice.id.clone(), session.clone(), key.clone());
                            toasts::show(
                                toasts::Notice {
                                    key,
                                    title: format!(
                                        "Channel setup couldn’t finish: {}",
                                        notice.name
                                    )
                                    .into(),
                                    description: Some(notice.error.clone().into()),
                                    tone: ToastTone::Warning,
                                    timeout: None,
                                    dismissible: false,
                                    actions: vec![toasts::Action::new("Dismiss", move |_, cx| {
                                        session.dismiss_setup_notice(&id);
                                        toasts::dismiss(&close, cx);
                                    })],
                                },
                                cx,
                            );
                        }
                        shown.retain(|id| notices.iter().any(|n| n.id == *id));
                        true
                    });
                    if !alive || creation.changed().await.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
    };
    follow(cx);
    cx.observe_global::<AppState>(follow).detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn invalid_drafts_cannot_be_created() {
        let mut draft = Draft::default();
        assert!(draft.errors().name.is_some(), "a name is required");
        draft.name = "  #release-notes ".into();
        assert!(!draft.errors().any(), "normalized before checking");
        draft.description = "Buzz session (x)".into();
        assert!(draft.errors().description.is_some());
        draft.description = "x".repeat(1001);
        assert!(draft.errors().description.is_some());
    }

    #[test]
    fn any_choice_makes_the_draft_dirty() {
        assert!(!Draft::default().dirty());
        for draft in [
            Draft {
                name: " ".into(),
                ..Draft::default()
            },
            Draft {
                description: "x".into(),
                ..Draft::default()
            },
            Draft {
                private: true,
                ..Draft::default()
            },
            Draft {
                temporary: true,
                ..Draft::default()
            },
            Draft {
                template: "t".into(),
                ..Draft::default()
            },
        ] {
            assert!(draft.dirty());
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn a_template_adds_its_agents_and_teams_agents_here_and_its_canvas() {
        let (ada, bo, elsewhere) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        // Agents in this community; `elsewhere` is in another one.
        let mine = [(ada.clone(), "Ada".into()), (bo.clone(), "Bo".into())];
        let team = |id: &str, members: &[&str]| Team {
            id: id.into(),
            name: id.into(),
            instructions: String::new(),
            members: strings(members),
        };
        let teams = [team("t1", &[&ada, &elsewhere]), team("t2", &[&bo, &ada])];
        let template = ChannelTemplate {
            id: "launch".into(),
            name: "Launch".into(),
            teams: strings(&["t1", "t2", "gone"]),
            agents: vec![elsewhere.clone(), bo.clone()],
            canvas: "# Goal".into(),
        };
        let setup = TemplateSetup::new(template, &mine, &teams);
        assert_eq!(
            template_note(&setup.names, &setup.canvas),
            "Adds Bo, Ada · Starts the Canvas"
        );
        assert_eq!(
            channel_setup(String::new(), Some(setup)),
            Some(ChannelSetup {
                agents: vec![bo, ada],
                canvas: "# Goal".into(),
                group_id: String::new(),
                template_id: "launch".into(),
            })
        );
    }

    #[test]
    fn a_template_that_adds_nothing_here_makes_a_plain_channel() {
        let template = ChannelTemplate {
            id: "empty".into(),
            name: "Empty".into(),
            teams: strings(&["gone"]),
            agents: Vec::new(),
            canvas: String::new(),
        };
        let setup = TemplateSetup::new(template, &[], &[]);
        assert_eq!(
            template_note(&setup.names, &setup.canvas),
            "None of its agents are in this community, and it has no Canvas."
        );
        assert_eq!(channel_setup(String::new(), Some(setup)), None);
        assert_eq!(channel_setup(String::new(), None), None);
        // A sidebar group still sends setup, naming the template.
        let template = ChannelTemplate {
            id: "empty".into(),
            name: "Empty".into(),
            teams: Vec::new(),
            agents: Vec::new(),
            canvas: String::new(),
        };
        let setup = TemplateSetup::new(template, &[], &[]);
        assert_eq!(
            channel_setup("g".into(), Some(setup)),
            Some(ChannelSetup {
                group_id: "g".into(),
                template_id: "empty".into(),
                ..Default::default()
            })
        );
    }
}
