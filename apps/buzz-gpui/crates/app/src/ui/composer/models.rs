//! Model and effort for the next run of each mentioned agent of the
//! viewer's that offers a choice (`ComposerModels.tsx`, `useMentionModels`):
//! a gauge button in the toolbar and its popover above the composer, with
//! a model menu and an effort slider per agent. Unpicked agents run with
//! their own; each send clears the picks.

use super::Composer;
use crate::agents2::{Agents2, Effort, Model, Models, type_of};
use buzz_relay_client::messages::RunPick;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, ButtonVariant, IconButton, IconButtonSize, IconName, Menu,
    MenuItem, MenuSize, Slider, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The model and effort picked for each mentioned agent, by pubkey.
pub type Picks = BTreeMap<String, RunPick>;

const TITLE: &str = "Model and effort for the next run";
const LOADING: &str = "Loading model and effort…";

/// A mentioned agent of the viewer's whose type offers a choice
/// (`MentionModelChoice`). `loading` until its type has listed them.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub pubkey: String,
    pub name: String,
    pub models: Models,
    pub loading: bool,
}

/// The pubkeys a lookup was for, each with its agent's type, name and
/// config when it is one of the viewer's agents of a registered type,
/// and that registration: a change to the agent or a type registered anew
/// asks again (`[snapshot, types, key]`).
type Key = Vec<(String, Option<(String, String, String, usize)>)>;

/// One lookup of the mentioned agents' choices.
struct Lookup {
    key: Key,
    choices: Vec<Choice>,
    /// One per choice; the first takes focus when the popover opens.
    focus: Vec<FocusHandle>,
    _task: Option<Task<()>>,
}

/// The composer's picks and the popover's state.
#[derive(Default)]
pub struct RunModels {
    /// Cleared by each send.
    pub picks: Picks,
    lookup: Option<Lookup>,
    open: bool,
    /// When a press outside last closed the popover.
    closed: Option<Instant>,
    /// The agent whose model menu is open.
    menu: Option<String>,
}

/// Whether a type's listing is a choice: several models, or effort levels.
pub fn offers_choice(models: &Models) -> bool {
    models.models.len() > 1 || models.models.iter().any(|model| !model.efforts.is_empty())
}

/// The model a run uses: the picked one, or else the agent's own.
pub fn run_model(models: &Models, picked: Option<&str>) -> Model {
    picked
        .and_then(|id| models.models.iter().find(|model| model.id == id))
        .or_else(|| {
            models
                .models
                .iter()
                .find(|model| model.id == models.current)
        })
        .or_else(|| models.models.first())
        .cloned()
        .unwrap_or(Model {
            id: String::new(),
            label: String::new(),
            efforts: Vec::new(),
            effort: None,
        })
}

/// What each pick asks, `<name>: ` first when there are several agents.
pub fn picked(choices: &[Choice], picks: &Picks) -> Vec<String> {
    choices
        .iter()
        .filter_map(|choice| {
            let pick = picks.get(&choice.pubkey).cloned().unwrap_or_default();
            let runs = run_model(&choice.models, pick.model.as_deref());
            let effort = runs
                .efforts
                .iter()
                .find(|effort| Some(&effort.id) == pick.effort.as_ref())
                .map(|effort| effort.label.clone());
            let text = [pick.model.is_some().then(|| runs.label.clone()), effort]
                .into_iter()
                .flatten()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            (!text.is_empty()).then(|| {
                if choices.len() > 1 {
                    format!("{}: {text}", choice.name)
                } else {
                    text
                }
            })
        })
        .collect()
}

/// The gauge's tooltip.
pub fn tooltip(picked: &[String]) -> String {
    if picked.is_empty() {
        TITLE.to_string()
    } else {
        format!("Next run: {}", picked.join(", "))
    }
}

/// `picks` with `next` for `pubkey`; an empty pick is none.
pub fn with_pick(picks: &Picks, pubkey: &str, next: RunPick) -> Picks {
    let mut picks = picks.clone();
    picks.remove(pubkey);
    if next.model.is_some() || next.effort.is_some() {
        picks.insert(pubkey.to_string(), next);
    }
    picks
}

/// Picking the agent's own model is no pick; an effort the new model lacks
/// falls back to its default.
pub fn choose_model(models: &Models, value: &RunPick, id: &str) -> RunPick {
    let next = models.models.iter().find(|model| model.id == id);
    let effort = value
        .effort
        .clone()
        .filter(|effort| next.is_some_and(|next| next.efforts.iter().any(|e| e.id == *effort)));
    RunPick {
        model: (id != models.current).then(|| id.to_string()),
        effort,
    }
}

/// The run's own effort is no pick.
pub fn choose_effort(
    efforts: &[Effort],
    resting: Option<&str>,
    value: &RunPick,
    at: usize,
) -> RunPick {
    let effort = efforts
        .get(at)
        .map(|effort| effort.id.clone())
        .filter(|effort| Some(effort.as_str()) != resting);
    RunPick {
        model: value.model.clone(),
        effort,
    }
}

/// Where the slider stands and what it reads: the picked effort, else the
/// one the run uses without a pick, else the first stop, muted, reading
/// "Default".
pub fn effort_stop(
    efforts: &[Effort],
    resting: Option<&str>,
    value: Option<&str>,
) -> (usize, bool, String) {
    let set = efforts
        .iter()
        .position(|effort| Some(effort.id.as_str()) == value);
    let own = efforts
        .iter()
        .position(|effort| Some(effort.id.as_str()) == resting);
    let at = set.or(own).unwrap_or(0);
    let known = set.is_some() || own.is_some();
    let level = if known {
        efforts
            .get(at)
            .map(|effort| effort.label.clone())
            .unwrap_or_default()
    } else {
        "Default".to_string()
    };
    (at, known, level)
}

/// Only agents still notified by the message are asked for a model or
/// effort.
pub fn sent_picks(picks: &Picks, recipients: &[String]) -> Picks {
    picks
        .iter()
        .filter(|(key, _)| recipients.contains(key))
        .map(|(key, pick)| (key.clone(), pick.clone()))
        .collect()
}

/// Lookup results: those that offer a choice, in mention order. A type
/// that could not list its models offers none.
fn settled(results: Vec<(String, String, String, Result<Models, String>)>) -> Vec<Choice> {
    results
        .into_iter()
        .filter_map(|(pubkey, name, kind, result)| match result {
            Ok(models) if offers_choice(&models) => Some(Choice {
                pubkey,
                name,
                models,
                loading: false,
            }),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, "Agent type {kind} could not list its models");
                None
            }
        })
        .collect()
}

impl RunModels {
    /// The choices of the current lookup.
    fn choices(&self) -> &[Choice] {
        self.lookup.as_ref().map_or(&[], |lookup| &lookup.choices)
    }

    fn looked_up(&self, key: &Key) -> bool {
        self.lookup
            .as_ref()
            .is_some_and(|lookup| lookup.key == *key)
    }

    /// Starts the lookup for `key`, showing `choices` (loading) meanwhile.
    fn begin(
        &mut self,
        key: Key,
        choices: Vec<Choice>,
        focus: Vec<FocusHandle>,
        task: Option<Task<()>>,
    ) {
        self.lookup = Some(Lookup {
            key,
            choices,
            focus,
            _task: task,
        });
    }

    /// Settles the lookup for `key`; one for another set is discarded.
    fn settle(&mut self, key: &Key, choices: Vec<Choice>) -> bool {
        let Some(lookup) = self.lookup.as_mut().filter(|lookup| lookup.key == *key) else {
            return false;
        };
        lookup.focus.truncate(choices.len());
        lookup.choices = choices;
        lookup._task = None;
        true
    }
}

impl Composer {
    /// The button shows while the composer writes a message, not an edit or
    /// a new conversation's first one, that has mention recipients.
    fn run_models_shown(&self) -> bool {
        self.editing.is_none() && !self.no_attachments
    }

    /// Looks up the mentioned agents' choices when the set of them, or
    /// what they are, changed. A new set never shows a previous lookup's
    /// choices: it shows loading rows until its own settle.
    pub(super) fn refresh_run_models(&mut self, cx: &mut Context<Self>) {
        let pubkeys: Vec<String> = if self.run_models_shown() {
            self.recipients(cx)
                .into_iter()
                .map(|(key, _)| key)
                .collect()
        } else {
            Vec::new()
        };
        if pubkeys.is_empty() {
            let state = &mut self.run_models;
            state.lookup = None;
            state.open = false;
            state.menu = None;
            return;
        }
        let service = Agents2::global(cx);
        let found: Vec<_> = pubkeys
            .into_iter()
            .map(|pubkey| {
                let agent = service
                    .as_ref()
                    .and_then(|service| service.read(cx).find(&pubkey).cloned());
                let registered = agent
                    .as_ref()
                    .and_then(|agent| type_of(&agent.agent_type, cx));
                (pubkey, agent.zip(registered))
            })
            .collect();
        let key: Key = found
            .iter()
            .map(|(pubkey, found)| {
                (
                    pubkey.clone(),
                    found.as_ref().map(|(agent, kind)| {
                        (
                            kind.key.to_string(),
                            agent.name.clone(),
                            agent.config.to_string(),
                            std::sync::Arc::as_ptr(&kind.kind) as *const () as usize,
                        )
                    }),
                )
            })
            .collect();
        if self.run_models.looked_up(&key) {
            return;
        }
        let mut choices = Vec::new();
        let mut pending = Vec::new();
        for (pubkey, found) in found {
            let Some((agent, kind)) = found else {
                continue;
            };
            if let Some(task) = kind.kind.models(&agent, cx) {
                choices.push(Choice {
                    pubkey: pubkey.clone(),
                    name: agent.name.clone(),
                    models: Models::default(),
                    loading: true,
                });
                pending.push((pubkey, agent.name, kind.key.to_string(), task));
            }
        }
        let focus = choices.iter().map(|_| cx.focus_handle()).collect();
        let lookup_key = key.clone();
        // Every lookup settles before any shows, as one.
        let task = (!pending.is_empty()).then(|| {
            cx.spawn(async move |this, cx| {
                let mut results = Vec::new();
                for (pubkey, name, kind, task) in pending {
                    results.push((pubkey, name, kind, task.await));
                }
                let choices = settled(results);
                let _ = this.update(cx, |this, cx| {
                    if this.run_models.settle(&lookup_key, choices) {
                        cx.notify();
                    }
                });
            })
        });
        self.run_models.begin(key, choices, focus, task);
    }

    fn pick_run(&mut self, pubkey: &str, next: RunPick, cx: &mut Context<Self>) {
        self.run_models.picks = with_pick(&self.run_models.picks, pubkey, next);
        cx.notify();
    }

    fn toggle_run_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = &mut self.run_models;
        if state.open {
            state.open = false;
            state.menu = None;
            self.focus(window, cx);
            cx.notify();
            return;
        }
        // The press outside that just closed it.
        if state
            .closed
            .take()
            .is_some_and(|at| at.elapsed() < Duration::from_millis(300))
        {
            return;
        }
        state.open = true;
        if let Some(first) = state
            .lookup
            .as_ref()
            .and_then(|lookup| lookup.focus.first())
        {
            first.focus(window, cx);
        }
        cx.notify();
    }

    fn close_run_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_models.open = false;
        self.run_models.menu = None;
        self.focus(window, cx);
        cx.notify();
    }

    /// The gauge button, when a mentioned agent offers a choice.
    pub(super) fn run_models_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let choices = self.run_models.choices();
        if !self.run_models_shown() || choices.is_empty() {
            return None;
        }
        let picked = picked(choices, &self.run_models.picks);
        let disabled = self.input_disabled(cx);
        Some(
            IconButton::new("run-models", IconName::Gauge)
                .size(IconButtonSize::Sm)
                .icon_size(18.)
                .selected(!picked.is_empty())
                .disabled(disabled)
                .tooltip(tooltip(&picked))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_run_models(window, cx)))
                .into_any_element(),
        )
    }

    /// The popover: no surface, just each agent's controls, above the
    /// composer at its end.
    pub(super) fn run_models_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = &self.run_models;
        let lookup = state.lookup.as_ref()?;
        if !state.open || lookup.choices.is_empty() || self.input_disabled(cx) {
            return None;
        }
        let named = lookup.choices.len() > 1;
        let rows: Vec<AnyElement> = lookup
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| self.agent_choice(choice, named, lookup.focus.get(index), cx))
            .collect();
        let popup = div()
            .id("run-models")
            .occlude()
            .flex()
            .flex_col()
            .gap(px(12.))
            .w(rems(20.))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if this.run_models.menu.take().is_some() {
                        cx.notify();
                    } else {
                        this.close_run_models(window, cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                // A press in the open model menu is the menu's.
                if this.run_models.menu.is_some() {
                    return;
                }
                this.run_models.open = false;
                this.run_models.closed = Some(Instant::now());
                cx.notify();
            }))
            .children(rows);
        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .child(
                    deferred(
                        anchored()
                            .anchor(Anchor::BottomRight)
                            .offset(point(px(0.), px(-8.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(popup),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    fn agent_choice(
        &self,
        choice: &Choice,
        named: bool,
        focus: Option<&FocusHandle>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let about = |text: String| {
            div()
                .min_w_0()
                .truncate()
                .text_caption()
                .text_color(c.text_subtle)
                .child(text)
        };
        if choice.loading {
            return div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .border_1()
                .border_color(c.border_standard)
                .rounded(buzz_ui::tokens::radius::ROW)
                .bg(c.surface_popover)
                .text_caption()
                .text_color(c.text_subtle)
                .when(named, |d| d.child(choice.name.clone()))
                .child(LOADING)
                .into_any_element();
        }
        let disabled = self.input_disabled(cx);
        let value = self
            .run_models
            .picks
            .get(&choice.pubkey)
            .cloned()
            .unwrap_or_default();
        let runs = run_model(&choice.models, value.model.as_deref());
        let model = if choice.models.models.len() > 1 {
            self.model_menu(choice, &runs, &value, disabled, cx)
        } else {
            about(runs.label.clone()).into_any_element()
        };
        let control = if runs.efforts.is_empty() {
            model
        } else {
            let (at, known, level) = effort_stop(
                &runs.efforts,
                runs.effort.as_deref(),
                value.effort.as_deref(),
            );
            let efforts = runs.efforts.clone();
            let resting = runs.effort.clone();
            let pubkey = choice.pubkey.clone();
            let mut slider = Slider::new(
                SharedString::from(format!("effort-{}", choice.pubkey)),
                runs.efforts.len(),
                at,
            )
            .unknown(!known)
            .disabled(disabled)
            .tab(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_w_0()
                    .child(model)
                    .child(div().flex_none().child(level)),
            )
            .on_change(cx.listener(move |this, at: &usize, _, cx| {
                let value = this
                    .run_models
                    .picks
                    .get(&pubkey)
                    .cloned()
                    .unwrap_or_default();
                let next = choose_effort(&efforts, resting.as_deref(), &value, *at);
                this.pick_run(&pubkey, next, cx);
            }));
            if let Some(focus) = focus {
                slider = slider.track_focus(focus);
            }
            slider.into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .when(named, |d| d.child(about(choice.name.clone())))
            .child(control)
            .into_any_element()
    }

    /// "Model for X's next run": the run's model, opening a menu of the
    /// models above it.
    fn model_menu(
        &self,
        choice: &Choice,
        runs: &Model,
        value: &RunPick,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let open = self.run_models.menu.as_deref() == Some(choice.pubkey.as_str());
        let toggle = choice.pubkey.clone();
        let trigger = Button::new(
            SharedString::from(format!("run-model-{}", choice.pubkey)),
            runs.label.clone(),
        )
        .variant(ButtonVariant::Ghost)
        .size(ButtonSize::Inline)
        .label_color(c.text_subtle)
        .icon_end(IconName::CaretDown)
        .disabled(disabled)
        .tooltip(format!("Model for {}'s next run", choice.name))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.run_models.menu = match this.run_models.menu.take() {
                Some(open) if open == toggle => None,
                _ => Some(toggle.clone()),
            };
            cx.notify();
        }));
        let menu = open.then(|| {
            let items = choice.models.models.iter().map(|model| {
                let models = choice.models.clone();
                let value = value.clone();
                let pubkey = choice.pubkey.clone();
                let id = model.id.clone();
                MenuItem::new(
                    SharedString::from(format!("run-model-{}-{}", choice.pubkey, model.id)),
                    model.label.clone(),
                )
                .size(MenuSize::Compact)
                .checked(model.id == runs.id)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_models.menu = None;
                    let next = choose_model(&models, &value, &id);
                    this.pick_run(&pubkey, next, cx);
                }))
            });
            div().absolute().top_0().right_0().child(
                deferred(
                    anchored()
                        .anchor(Anchor::BottomRight)
                        .offset(point(px(0.), px(-4.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            div()
                                .id("run-model-menu")
                                .occlude()
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.run_models.menu = None;
                                    cx.notify();
                                }))
                                .child(
                                    Menu::new().size(MenuSize::Compact).child(
                                        // A long list scrolls within the menu.
                                        div()
                                            .id("run-model-items")
                                            .flex()
                                            .flex_col()
                                            .max_h(rems(23.5))
                                            .overflow_y_scroll()
                                            .children(items),
                                    ),
                                ),
                        ),
                )
                .with_priority(2),
            )
        });
        div()
            .relative()
            .min_w_0()
            .child(trigger)
            .children(menu)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests;
