//! The landing (`WorkflowLanding.tsx`, `WorkflowDeletionNotices.tsx`):
//! every participating channel's workflows as cards, discovered in
//! batches, with the deletion recovery that stays on the grid.

use super::controls::{self, Confirm};
use super::model::activation;
use super::model::card::{CardIcon, Presentation, presentation};
use super::model::editor::{confirmed_deletion, operation_locked};
use super::model::yaml::{self as document, DocumentFields};
use crate::mirror::mirror;
use crate::runtime::Runtime;
use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::toasts;
use buzz_relay_client::ChannelSummary;
use buzz_relay_client::workflows::{
    Action, Landing, LandingSnapshot, Outcome, ViewStatus, WorkflowDefinition, WorkflowOperation,
    Workflows,
};
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, Icon, IconButton, IconButtonSize, IconName, Menu, MenuItem,
    MenuSize, Switch, ToastTone, TypeStyled,
    tokens::{radius, shadow, space},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

// Short-lived events and dialogs; boxing buys nothing here.
#[allow(clippy::large_enum_variant)]
pub enum LandingEvent {
    Open(WorkflowDefinition, ChannelSummary, bool),
    Delete(WorkflowDefinition),
    Create,
}

impl EventEmitter<LandingEvent> for LandingView {}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum CardConfirm {
    Delete(WorkflowDefinition),
    Enable(WorkflowDefinition, ChannelSummary),
}

pub struct LandingView {
    workflows: Workflows,
    viewer: String,
    channels: Vec<ChannelSummary>,
    landing: Landing,
    snapshot: Arc<LandingSnapshot>,
    operations: Arc<[WorkflowOperation]>,
    refresh_key: String,
    menu: Option<String>,
    confirm: Option<CardConfirm>,
    /// Saves submitted from a card's switch, by card.
    submitted: HashMap<String, String>,
    /// A card error per channel, until dismissed.
    errors: HashMap<String, String>,
    /// The deletion notice being dismissed.
    acknowledge: Option<WorkflowOperation>,
    working: bool,
    notice_error: Option<String>,
    /// Notices up, by key, with what they say.
    toasts: HashMap<String, String>,
    width: Rc<Cell<Pixels>>,
    confirm_focus: FocusHandle,
    /// Each shown card's parsed YAML, by revision.
    parsed: HashMap<String, Rc<CardParse>>,
    _watch: Vec<Task<()>>,
}

/// What a card shows of its definition's YAML, parsed once per revision.
struct CardParse {
    fields: DocumentFields,
    card: Presentation,
    warning: Option<activation::Warning>,
}

impl CardParse {
    fn new(yaml: &str) -> Self {
        Self {
            fields: document::read_fields(yaml),
            card: presentation(yaml),
            warning: activation::warning(yaml),
        }
    }
}

/// `.workflow-card-grid` columns: 2 from 42rem, 3 from 63rem.
fn columns(width: Pixels, rem: Pixels) -> usize {
    let rems = f32::from(width) / f32::from(rem);
    if rems >= 63. {
        3
    } else if rems >= 42. {
        2
    } else {
        1
    }
}

fn card_icon(icon: CardIcon) -> IconName {
    match icon {
        CardIcon::Delay => IconName::Timer,
        CardIcon::Diff => IconName::GitPullRequest,
        CardIcon::Message => IconName::ChatCircle,
        CardIcon::Reaction => IconName::Smiley,
        CardIcon::Schedule => IconName::Calendar,
        CardIcon::Webhook => IconName::WebhooksLogo,
        CardIcon::Workflow => IconName::Lightning,
    }
}

/// `toLocaleDateString()` in en-US: `11/14/2023`.
fn date(seconds: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(seconds as i64, 0)
        .single()
        .map(|time| time.format("%-m/%-d/%Y").to_string())
        .unwrap_or_default()
}

fn card_key(definition: &WorkflowDefinition) -> String {
    format!(
        "{}:{}:{}",
        definition.owner, definition.id, definition.revision
    )
}

impl LandingView {
    pub fn new(
        workflows: Workflows,
        viewer: String,
        channels: Vec<ChannelSummary>,
        cx: &mut Context<Self>,
    ) -> Self {
        let landing = Landing::new(workflows.clone(), Runtime::handle(cx));
        let mut this = Self {
            snapshot: landing.snapshot(),
            operations: workflows.operations(),
            _watch: vec![
                mirror(landing.watch(), cx, |this: &mut Self, value| {
                    this.snapshot = value;
                }),
                mirror(
                    workflows.watch_operations(),
                    cx,
                    |this: &mut Self, value| {
                        this.operations = value;
                        this.operations_changed();
                    },
                ),
            ],
            workflows,
            viewer,
            channels: Vec::new(),
            landing,
            refresh_key: String::new(),
            menu: None,
            confirm: None,
            submitted: HashMap::new(),
            errors: HashMap::new(),
            acknowledge: None,
            working: false,
            notice_error: None,
            toasts: HashMap::new(),
            width: Rc::new(Cell::new(px(0.))),
            confirm_focus: cx.focus_handle(),
            parsed: HashMap::new(),
        };
        this.set_channels(channels);
        this
    }

    /// The participating channels; new ones are read, gone ones dropped.
    pub fn set_channels(&mut self, channels: Vec<ChannelSummary>) {
        let mut ids: Vec<String> = channels.iter().map(|c| c.id.clone()).collect();
        let mut previous: Vec<String> = self.channels.iter().map(|c| c.id.clone()).collect();
        ids.sort();
        previous.sort();
        self.channels = channels;
        if ids != previous {
            self.landing.channels(ids);
        }
    }

    /// "Refresh workflows": rereads every channel.
    pub fn refresh(&self) {
        self.landing.refresh(None, false);
    }

    /// Verified editor readback also refreshes that channel on the landing,
    /// when the copied result lacks the saved revision.
    pub fn readback(&self, channel: &str, revision: &str) {
        let shown = self
            .snapshot
            .snapshots
            .get(channel)
            .is_some_and(|s| s.data.items.iter().any(|d| d.revision == revision));
        if !shown {
            self.landing.refresh(Some(vec![channel.to_string()]), true);
        }
    }

    /// Saves and deletions that resolved or became uncertain reread their
    /// channels once.
    fn operations_changed(&mut self) {
        let key = self
            .operations
            .iter()
            .filter(|op| {
                matches!(op.action, Action::Save | Action::Delete)
                    && matches!(op.outcome, Outcome::Unknown | Outcome::Succeeded)
            })
            .map(|op| {
                let mut key = format!(
                    "{}/{}/{}",
                    op.workflow.channel_id,
                    op.event_id,
                    op.outcome.as_str()
                );
                if op.action == Action::Delete {
                    key.push_str(&format!(
                        "/{}/{}",
                        buzz_relay_client::workflows::delivery_label(op.delivery),
                        op.error.is_some()
                    ));
                }
                key
            })
            .collect::<Vec<_>>()
            .join(":");
        if key != self.refresh_key {
            self.refresh_key = key.clone();
            if !key.is_empty() {
                let ids: Vec<String> = key
                    .split(':')
                    .filter_map(|part| part.split('/').next())
                    .map(str::to_owned)
                    .collect();
                self.landing.refresh(Some(ids), true);
            }
        }
    }

    fn dismiss_menu(cx: &mut Context<Self>) -> Dismiss {
        let this = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = this.update(cx, |this, cx| {
                this.menu = None;
                cx.notify();
            });
        })
    }

    fn toggle(
        &mut self,
        definition: &WorkflowDefinition,
        channel: &ChannelSummary,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(yaml) = document::with_enabled(&definition.yaml, enabled) else {
            self.errors.insert(
                channel.id.clone(),
                "This workflow cannot be changed from the landing page.".into(),
            );
            cx.notify();
            return;
        };
        match self.workflows.save(&channel.id, yaml, Some(definition)) {
            Ok(id) => {
                self.submitted.insert(card_key(definition), id);
                self.errors.remove(&channel.id);
            }
            Err(error) => {
                self.errors.insert(channel.id.clone(), error);
            }
        }
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn card(
        &self,
        definition: &WorkflowDefinition,
        channel: &ChannelSummary,
        parsed: &CardParse,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let availability = self.workflows.availability();
        let deletion = self.operations.iter().rev().find(|op| {
            op.action == Action::Delete
                && op.outcome != Outcome::Rejected
                && definition.names(&op.workflow)
        });
        let (fields, card) = (&parsed.fields, &parsed.card);
        let enabled = fields.is_enabled();
        let readonly = definition.owner != self.viewer;
        let locked = operation_locked(&self.operations, definition);
        let key = card_key(definition);
        let submitted = self.submitted.get(&key);
        let submitted_op =
            submitted.and_then(|id| self.operations.iter().find(|op| &op.event_id == id));
        let awaiting = submitted.is_some_and(|id| {
            &definition.revision != id
                && submitted_op.is_none_or(|op| op.outcome != Outcome::Rejected)
        });
        let toggle_reason = if readonly {
            Some("Only the author can change this workflow.")
        } else if locked || awaiting {
            Some("Waiting for the submitted change to be resolved.")
        } else if !fields.editable {
            Some("Correct the YAML before changing the configured state.")
        } else if !availability.save {
            Some("Saving is unavailable from this host.")
        } else {
            None
        };
        let warning = &parsed.warning;
        let name = fields
            .name
            .clone()
            .unwrap_or_else(|| "Unnamed or malformed workflow".into());
        let menu_open = self.menu.as_deref() == Some(&key);
        let chip = |content: AnyElement| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(rems(2.25))
                .border_1()
                .border_color(c.border_standard)
                .rounded(radius::CONTROL)
                .bg(c.surface_panel)
                .shadow(shadow::xs())
                .child(content)
        };
        let trigger = match &card.trigger_emoji {
            Some(emoji) => div().text_body_lg().child(emoji.clone()).into_any_element(),
            None => Icon::new(card_icon(card.trigger_icon))
                .size(20.)
                .stroke_width(2.5)
                .into_any_element(),
        };
        let mut flow = div()
            .flex()
            .items_center()
            .gap(space::S2)
            .min_w_0()
            .child(chip(trigger).text_color(c.text_standard));
        if !card.actions.is_empty() {
            let mut stack = div().flex().items_center();
            for (index, (icon, _)) in card.actions.iter().take(3).enumerate() {
                let color = match index {
                    0 => c.text_standard,
                    1 => c.text_subtle,
                    _ => c.text_metadata,
                };
                stack = stack.child(
                    chip(
                        Icon::new(card_icon(*icon))
                            .size(20.)
                            .stroke_width(2.5)
                            .into_any_element(),
                    )
                    .text_color(color)
                    .when(index > 0, |this| this.ml(rems(-0.5))),
                );
            }
            flow = flow
                .child(
                    Icon::new(IconName::ArrowRight)
                        .size(16.)
                        .color(c.text_metadata),
                )
                .child(stack);
        }
        let open_definition = definition.clone();
        let open_channel = channel.clone();
        let run_definition = definition.clone();
        let run_channel = channel.clone();
        let delete_definition = definition.clone();
        let menu_key = key.clone();
        let menu = Menu::new()
            .size(MenuSize::Compact)
            .child(
                MenuItem::new(
                    "card-open",
                    if readonly {
                        "View workflow"
                    } else {
                        "Edit workflow"
                    },
                )
                .icon(if readonly {
                    IconName::Eye
                } else {
                    IconName::PencilSimple
                })
                .icon_size(14.)
                .size(MenuSize::Compact)
                .disabled(deletion.is_some())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    cx.emit(LandingEvent::Open(
                        open_definition.clone(),
                        open_channel.clone(),
                        false,
                    ));
                })),
            )
            .child(
                MenuItem::new("card-run", "Run now")
                    .icon(IconName::Play)
                    .icon_size(14.)
                    .size(MenuSize::Compact)
                    .disabled(readonly || locked || awaiting || !availability.trigger)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        cx.emit(LandingEvent::Open(
                            run_definition.clone(),
                            run_channel.clone(),
                            true,
                        ));
                    })),
            )
            .child(
                MenuItem::new("card-delete", "Delete workflow")
                    .icon(IconName::Trash)
                    .icon_size(14.)
                    .size(MenuSize::Compact)
                    .danger()
                    .disabled(readonly || locked || awaiting || !availability.delete)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        this.confirm = Some(CardConfirm::Delete(delete_definition.clone()));
                        cx.notify();
                    })),
            );
        let switch_definition = definition.clone();
        let switch_channel = channel.clone();
        let has_warning = warning.is_some();
        let this_entity = cx.entity().downgrade();
        let controls_row = div()
            .flex()
            .items_center()
            .gap(space::S2)
            .flex_none()
            .when(fields.editable, |this| {
                this.child(
                    Switch::new(SharedString::from(format!("card-switch-{key}")), enabled)
                        .disabled(toggle_reason.is_some())
                        .on_change(move |next, _, cx| {
                            let definition = switch_definition.clone();
                            let channel = switch_channel.clone();
                            let _ = this_entity.update(cx, |this, cx| {
                                if next == enabled {
                                    return;
                                }
                                if next && has_warning {
                                    this.confirm = Some(CardConfirm::Enable(definition, channel));
                                    cx.notify();
                                } else {
                                    this.toggle(&definition, &channel, next, cx);
                                }
                            });
                        }),
                )
            })
            .child(
                div()
                    .relative()
                    .child(
                        IconButton::new(
                            SharedString::from(format!("card-actions-{key}")),
                            IconName::DotsThree,
                        )
                        .size(IconButtonSize::Sm)
                        .selected(menu_open)
                        .when(!menu_open, |b| b.tooltip(format!("Actions for {name}")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = if this.menu.as_deref() == Some(&menu_key) {
                                None
                            } else {
                                Some(menu_key.clone())
                            };
                            cx.notify();
                        })),
                    )
                    .when(menu_open, |this| {
                        this.child(overlay::dropdown(
                            window,
                            Align::End,
                            menu,
                            Self::dismiss_menu(cx),
                        ))
                    }),
            );
        let status = deletion.map(|op| {
            if op.outcome == Outcome::Pending {
                "Deleting…"
            } else {
                "Deletion unconfirmed"
            }
        });
        let card_definition = definition.clone();
        let card_channel = channel.clone();
        let clickable = deletion.is_none();
        let hover = c.affordance_subtle_hover;
        div()
            .id(SharedString::from(format!("workflow-card-{key}")))
            .relative()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h(rems(15.))
            .p(space::S5)
            .rounded(radius::CONTROL)
            .bg(c.surface_inset)
            .shadow(shadow::xs())
            .overflow_hidden()
            .when(clickable, |this| {
                this.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(LandingEvent::Open(
                            card_definition.clone(),
                            card_channel.clone(),
                            false,
                        ));
                    }))
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_between()
                    .gap(space::S3)
                    .child(flow)
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(controls_row),
                    ),
            )
            .child(
                div()
                    .max_w(rems(24.))
                    .my(space::S5)
                    .text_body_lg()
                    .font_weight(FontWeight::MEDIUM)
                    .line_clamp(4)
                    .child(card.description.clone()),
            )
            .child(
                div()
                    .mt_auto()
                    .flex()
                    .items_end()
                    .justify_between()
                    .gap(space::S3)
                    .text_caption()
                    .text_color(c.text_subtle)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::HALF)
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(c.text_standard)
                                    .child(format!("#{}", channel.name)),
                            )
                            .child(div().truncate().child(name.clone()))
                            .children(status.map(|status| div().truncate().child(status))),
                    )
                    .child(div().flex_none().child(date(definition.created_at))),
            )
            .when(!fields.editable, |this| {
                this.child(
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child("Unreadable configuration"),
                )
            })
            .into_any_element()
    }

    /// Deletion notices go to the toast stack (`ToastNotice`), recovery
    /// included; they update in place and leave once resolved.
    fn sync_notices(&mut self, cx: &mut Context<Self>) {
        let mut shown: HashMap<String, String> = HashMap::new();
        let operations = self.operations.clone();
        for operation in operations.iter().filter(|op| {
            op.action == Action::Delete
                // A newer attempt that was not rejected supersedes a failure.
                && (op.outcome != Outcome::Rejected
                    || !operations.iter().any(|other| {
                        other.action == Action::Delete
                            && other.outcome != Outcome::Rejected
                            && other.workflow == op.workflow
                    }))
        }) {
            let snapshot = self.snapshot.snapshots.get(&operation.workflow.channel_id);
            let definition = snapshot.and_then(|s| {
                s.data.items.iter().find(|row| {
                    row.id == operation.workflow.id && row.owner == operation.workflow.owner
                })
            });
            let key = format!("workflow-deletion-{}", operation.event_id);
            let notice = if operation.outcome == Outcome::Pending {
                if definition.is_some() {
                    continue;
                }
                toasts::Notice {
                    key: key.clone().into(),
                    title: "Deleting workflow…".into(),
                    description: Some("Waiting for deletion confirmation.".into()),
                    tone: ToastTone::Info,
                    timeout: None,
                    dismissible: false,
                    actions: Vec::new(),
                }
            } else {
                let Some(snapshot) = snapshot else { continue };
                if matches!(snapshot.status, ViewStatus::Idle | ViewStatus::Unavailable)
                    || confirmed_deletion(operation, Some(snapshot))
                {
                    continue;
                }
                let rejected = operation.outcome == Outcome::Rejected;
                let name = definition.and_then(|d| document::read_fields(&d.yaml).name);
                let checking = snapshot.status == ViewStatus::Loading;
                let landing = cx.entity().downgrade();
                let check_channel = operation.workflow.channel_id.clone();
                let check = landing.clone();
                let acknowledge = operation.clone();
                let mut actions = Vec::new();
                if !checking || !self.working {
                    actions.push(toasts::Action::new(
                        if checking {
                            "Checking deletion…"
                        } else {
                            "Check saved configuration"
                        },
                        move |_, cx| {
                            let channel = check_channel.clone();
                            let _ = check.update(cx, |this, _| {
                                if !this.working {
                                    this.landing.refresh(Some(vec![channel]), false);
                                }
                            });
                        },
                    ));
                }
                actions.push(toasts::Action::new("Dismiss notice", move |_, cx| {
                    let acknowledge = acknowledge.clone();
                    let _ = landing.update(cx, |this, cx| {
                        if !this.working {
                            this.notice_error = None;
                            this.acknowledge = Some(acknowledge);
                            cx.notify();
                        }
                    });
                }));
                let mut description = if rejected {
                    "Check the workflow and try again from its menu.".to_string()
                } else {
                    "Check saved configuration before deleting again. Work already running may continue.".to_string()
                };
                if let Some(error) = &operation.error {
                    description.push_str(&format!("\n\nDelivery details: {error}"));
                }
                toasts::Notice {
                    key: key.clone().into(),
                    title: if rejected {
                        format!("Couldn't delete {}", name.as_deref().unwrap_or("workflow"))
                    } else {
                        match &name {
                            Some(name) => format!("Couldn't confirm deletion of {name}"),
                            None => "Couldn't confirm deletion".into(),
                        }
                    }
                    .into(),
                    description: Some(description.into()),
                    tone: ToastTone::Error,
                    timeout: None,
                    dismissible: false,
                    actions,
                }
            };
            let said = format!(
                "{}|{:?}|{}",
                notice.title,
                notice.description,
                notice
                    .actions
                    .iter()
                    .map(|a| a.label.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            if self.toasts.get(&key) != Some(&said) {
                toasts::show(notice, cx);
            }
            shown.insert(key, said);
        }
        for key in self.toasts.keys() {
            if !shown.contains_key(key) {
                toasts::dismiss(key, cx);
            }
        }
        self.toasts = shown;
        // A cleared or unavailable channel withdraws its dismissal.
        if let Some(op) = &self.acknowledge
            && self
                .snapshot
                .snapshots
                .get(&op.workflow.channel_id)
                .is_none_or(|s| matches!(s.status, ViewStatus::Idle | ViewStatus::Unavailable))
        {
            self.acknowledge = None;
            self.notice_error = None;
        }
    }

    fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        let Some(operation) = self.acknowledge.clone() else {
            return;
        };
        if self.working {
            return;
        }
        self.working = true;
        let work = Runtime::handle(cx).spawn(self.workflows.dismiss(&operation.event_id));
        cx.spawn(async move |this, cx| {
            let result = work
                .await
                .unwrap_or_else(|_| Err("Couldn't dismiss this notice. Try again.".into()));
            let _ = this.update(cx, |this, cx| {
                this.working = false;
                match result {
                    Ok(()) => {
                        this.acknowledge = None;
                        this.notice_error = None;
                    }
                    Err(error) => this.notice_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entity = cx.entity().downgrade();
        let cancel = {
            let entity = entity.clone();
            Rc::new(move |_: &mut Window, cx: &mut App| {
                let _ = entity.update(cx, |this, cx| {
                    this.confirm = None;
                    cx.notify();
                });
            })
        };
        if let Some(operation) = self.acknowledge.clone() {
            let cancel_notice = {
                let entity = entity.clone();
                Rc::new(move |_: &mut Window, cx: &mut App| {
                    let _ = entity.update(cx, |this, cx| {
                        if !this.working {
                            this.acknowledge = None;
                        }
                        cx.notify();
                    });
                })
            };
            let _ = operation;
            return Some(
                Confirm::new(
                    "Dismiss this notice?",
                    "Dismissing this notice unlocks the workflow. It does not confirm, cancel, or repeat deletion. Check saved configuration before deleting again.",
                    if self.working {
                        "Dismissing…"
                    } else {
                        "Dismiss notice and continue"
                    },
                )
                .pending(self.working)
                .error(self.notice_error.clone().map(SharedString::from))
                .render(
                    "deletion-dismiss",
                    &self.confirm_focus,
                    Rc::new(move |_, cx| {
                        let _ = entity.update(cx, |this, cx| this.dismiss_notice(cx));
                    }),
                    cancel_notice,
                    window,
                    cx,
                ),
            );
        }
        match self.confirm.clone()? {
            CardConfirm::Delete(definition) => Some(
                Confirm::new(
                    "Delete this workflow?",
                    "The saved workflow may remain visible. Work already running may continue.",
                    "Delete workflow",
                )
                .cancel("Cancel")
                .destructive()
                .render(
                    "card-delete",
                    &self.confirm_focus,
                    Rc::new(move |_, cx| {
                        let definition = definition.clone();
                        let _ = entity.update(cx, |this, cx| {
                            this.confirm = None;
                            cx.emit(LandingEvent::Delete(definition));
                            cx.notify();
                        });
                    }),
                    cancel,
                    window,
                    cx,
                ),
            ),
            CardConfirm::Enable(definition, channel) => {
                let warning = activation::warning(&definition.yaml);
                Some(
                    Confirm::new(
                        warning
                            .as_ref()
                            .map_or("Turn on this workflow?", |w| w.title),
                        warning.as_ref().map_or_else(
                            || {
                                "The relay may run this workflow as soon as its trigger matches."
                                    .to_string()
                            },
                            |w| w.description.clone(),
                        ),
                        "Turn on",
                    )
                    .cancel("Keep off")
                    .render(
                        "card-enable",
                        &self.confirm_focus,
                        Rc::new(move |_, cx| {
                            let definition = definition.clone();
                            let channel = channel.clone();
                            let _ = entity.update(cx, |this, cx| {
                                this.confirm = None;
                                this.toggle(&definition, &channel, true, cx);
                            });
                        }),
                        cancel,
                        window,
                        cx,
                    ),
                )
            }
        }
    }
}

impl Drop for LandingView {
    fn drop(&mut self) {
        self.landing.dispose();
    }
}

impl Render for LandingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_notices(cx);
        let c = *cx.colors();
        let availability = self.workflows.availability();
        let snapshot = self.snapshot.clone();
        let status = if snapshot.paused {
            "Workflow discovery paused. Some channels could not be checked. Loaded workflows are still shown."
        } else if self.channels.iter().any(|channel| {
            snapshot
                .snapshots
                .get(&channel.id)
                .is_none_or(|s| s.status == ViewStatus::Loading)
        }) {
            "Reading workflows…"
        } else if self.channels.iter().any(|channel| {
            snapshot
                .snapshots
                .get(&channel.id)
                .is_some_and(|s| matches!(s.status, ViewStatus::Idle | ViewStatus::Unavailable))
        }) {
            "Workflow data cleared or unavailable. Refresh to check access."
        } else {
            "Workflows loaded."
        };
        let notice = div()
            .flex()
            .flex_col()
            .gap(px(0.))
            .child(controls::note(
                "Saved configuration only. Turning off does not confirm runs have stopped or cancel active runs.",
                cx,
            ))
            .child(controls::note(status, cx))
            .when(snapshot.paused, |this| {
                this.child(
                    div().flex().child(
                        Button::new("landing-retry", "Retry")
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, _| this.landing.retry())),
                    ),
                )
            });
        // Grid: the create card, then each channel's cards.
        let mut cells: Vec<AnyElement> = Vec::new();
        let border = c.border_prominent;
        let hover = c.affordance_subtle_hover;
        let can_create = availability.save && !self.channels.is_empty();
        cells.push(
            div()
                .id("workflow-create-card")
                .flex()
                .items_center()
                .justify_center()
                .w_full()
                .min_h(rems(15.))
                .border_1()
                .border_dashed()
                .border_color(border)
                .rounded(radius::CONTROL)
                .text_color(if can_create {
                    c.text_subtle
                } else {
                    c.text_unavailable
                })
                .when(can_create, |this| {
                    this.cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(LandingEvent::Create)))
                })
                .child(Icon::new(IconName::Plus).size(28.).stroke_width(2.5))
                .into_any_element(),
        );
        let channels = self.channels.clone();
        let mut shown = HashSet::new();
        for channel in &channels {
            let Some(snapshot) = snapshot.snapshots.get(&channel.id) else {
                continue;
            };
            if matches!(snapshot.status, ViewStatus::Idle | ViewStatus::Unavailable) {
                continue;
            }
            if let Some(error) = self.errors.get(&channel.id).cloned() {
                let id = channel.id.clone();
                cells.push(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .justify_center()
                        .gap(space::S4)
                        .p(space::S5)
                        .min_h(rems(15.))
                        .rounded(radius::CONTROL)
                        .bg(c.surface_inset)
                        .shadow(shadow::xs())
                        .child(div().text_body_sm().text_color(c.text_danger).child(error))
                        .child(
                            Button::new(SharedString::from(format!("card-error-{id}")), "Dismiss")
                                .size(ButtonSize::Sm)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.errors.remove(&id);
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
            for definition in &snapshot.data.items {
                let parsed = self
                    .parsed
                    .entry(definition.revision.clone())
                    .or_insert_with(|| Rc::new(CardParse::new(&definition.yaml)))
                    .clone();
                shown.insert(definition.revision.clone());
                cells.push(self.card(definition, channel, &parsed, window, cx));
            }
            if snapshot.data.partial {
                cells.push(
                    div()
                        .flex()
                        .items_center()
                        .p(space::S5)
                        .min_h(rems(15.))
                        .rounded(radius::CONTROL)
                        .bg(c.surface_inset)
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child(format!(
                            "#{} returned a partial workflow list.",
                            channel.name
                        ))
                        .into_any_element(),
                );
            }
        }
        self.parsed.retain(|revision, _| shown.contains(revision));
        let columns = columns(self.width.get(), window.rem_size());
        let width = self.width.clone();
        let entity = cx.entity().downgrade();
        let rem = window.rem_size();
        let measure = canvas(
            move |bounds, _, cx| {
                let old = width.replace(bounds.size.width);
                if columns_changed(old, bounds.size.width, rem) {
                    let _ = entity.update(cx, |_, cx| cx.notify());
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let mut grid = div()
            .relative()
            .flex()
            .flex_col()
            .gap(space::S3)
            .mt(space::S8)
            .child(measure);
        let mut row = Vec::new();
        for cell in cells {
            row.push(div().flex_1().min_w_0().child(cell));
            if row.len() == columns {
                grid = grid.child(
                    div()
                        .flex()
                        .gap(space::S3)
                        .children(std::mem::take(&mut row)),
                );
            }
        }
        if !row.is_empty() {
            while row.len() < columns {
                row.push(div().flex_1().min_w_0());
            }
            grid = grid.child(div().flex().gap(space::S3).children(row));
        }
        let confirmation = self.confirmation(window, cx);
        div()
            .flex()
            .flex_col()
            .child(notice)
            .child(grid)
            .children(confirmation)
    }
}

fn columns_changed(old: Pixels, new: Pixels, rem: Pixels) -> bool {
    columns(old, rem) != columns(new, rem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn lays_out_one_to_three_columns() {
        let rem = px(16.);
        assert_eq!(columns(px(600.), rem), 1);
        assert_eq!(columns(px(672.), rem), 2);
        assert_eq!(columns(px(1008.), rem), 3);
    }
}
