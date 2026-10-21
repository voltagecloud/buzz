//! The type's tabs (buzz-app's `tabs.ts`): Codex, with Codex's status on
//! the agent's host and the agent's conversations; and Settings, what the
//! agent runs with. Both read the account and models through a short-lived
//! `codex app-server` on that host.

use super::config::{Config, DEFAULT_WORKSPACE, Model, effort_name, valid_workspace};
use super::rpc::{AppServer, list_models};
use super::runtime::{CodexRuntime, SavedConversation, SessionView};
use super::setup::{CodexSetup, CodexStatus, SetupState, Step};
use super::transcript::{Lookup, SharedNames, TURN_LIMIT, read_transcript};
use crate::agent_plugins::opened::{self, CONVERSATION_LIMIT, CardSession, Found, Place, places};
use crate::agent_plugins::process::Spawner;
use crate::agent_plugins::tabs::{
    Options, Picking, choice, description_text, field, instructions_field, instructions_state,
    model_picker, note_with_code, saved_config,
};
use crate::agent_plugins::transcript_view::{self, Back, Loaded, Reader, Texts, TranscriptView};
use crate::agents2::{AgentConversation, AgentView, Agents2, ModelChoice};
use crate::buzz_mcp::app_client::Signal;
use crate::runtime::Runtime;
use crate::ui::agent_activity::panel::date_time;
use buzz_ui::{ActiveTheme, Button, Colors, Surface, TextField, TypeStyled, tokens::space};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::{Value, json};
use std::rc::Rc;
use std::sync::Arc;

/// What `account/read` says when nobody is signed in.
pub const SIGN_IN: &str = "Codex needs you to sign in.";
/// What the check says when Buzz has no copy of Codex on the host.
pub const NOT_INSTALLED: &str = "Codex is not installed.";

/// The account Codex uses and the models it offers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    pub models: Vec<Model>,
    pub account: String,
}

/// Reads the account and models with a server of its own, closed when
/// done or when `signal` aborts. Must run inside tokio.
pub async fn catalog(spawn: Spawner, signal: Signal) -> Result<Catalog, String> {
    let rpc = AppServer::new(None);
    let closer = {
        let (rpc, signal) = (rpc.clone(), signal.clone());
        tokio::spawn(async move {
            signal.wait().await;
            rpc.close().await;
        })
    };
    let result = async {
        signal.check()?;
        rpc.open(&spawn, None).await?;
        signal.check()?;
        let read = rpc.request("account/read", json!({})).await?;
        let Some(account) = read.get("account").filter(|account| account.is_object()) else {
            return Err(SIGN_IN.to_string());
        };
        let models = list_models(&rpc).await?;
        let text = |name: &str| {
            account
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Ok(Catalog {
            models,
            account: text("email").or_else(|| text("type")).unwrap_or_default(),
        })
    }
    .await;
    closer.abort();
    rpc.close().await;
    result
}

/// Where the agent's processes run, and Buzz's copy of Codex there.
#[derive(Clone)]
pub struct Host {
    pub spawn: Option<Spawner>,
    pub setup: CodexSetup,
}

/// One check of Codex on the agent's host (buzz-app's `useCatalog`): a
/// tab's own, stopped when the tab goes away.
pub struct CatalogCheck {
    host: Host,
    pub data: Option<Catalog>,
    pub error: String,
    pub checking: bool,
    /// The copy's install or sign-in, as it runs.
    pub setup: SetupState,
    signal: Signal,
    _task: Option<Task<()>>,
    _follow: Option<Task<()>>,
}

impl CatalogCheck {
    pub fn new(host: Host, cx: &mut Context<Self>) -> Self {
        // An install or sign-in holds the tab, then Codex is checked again.
        let mut changes = host.setup.subscribe();
        let follow = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let state = changes.borrow_and_update().clone();
                let updated = this.update(cx, |this, cx| {
                    let ended = this.setup.running.is_some() && state.running.is_none();
                    this.setup = state;
                    if ended {
                        this.retry(cx);
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        });
        let mut this = Self {
            setup: host.setup.snapshot(),
            host,
            data: None,
            error: String::new(),
            checking: true,
            signal: Signal::default(),
            _task: None,
            _follow: Some(follow),
        };
        this.retry(cx);
        this
    }

    /// Checks again, abandoning a check in flight.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.signal.abort("Codex check replaced");
        self.signal = Signal::default();
        self.checking = true;
        self.error.clear();
        cx.notify();
        let Some(spawn) = self.host.spawn.clone() else {
            self.checking = false;
            self.data = None;
            self.error = "The agent's host is not connected".into();
            return;
        };
        let signal = self.signal.clone();
        let setup = self.host.setup.clone();
        let check = Runtime::handle(cx).spawn({
            let signal = signal.clone();
            async move {
                match setup.check().await {
                    CodexStatus::Ready { .. } => catalog(spawn, signal).await,
                    CodexStatus::Missing => Err(NOT_INSTALLED.into()),
                    CodexStatus::SignedOut { .. } | CodexStatus::Outdated { .. } => {
                        Err(SIGN_IN.into())
                    }
                    CodexStatus::Error { message } => Err(message),
                }
            }
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = check
                .await
                .unwrap_or_else(|_| Err("Codex check stopped".into()));
            let _ = this.update(cx, |this, cx| {
                if signal.aborted() {
                    return;
                }
                match result {
                    Ok(catalog) => this.data = Some(catalog),
                    Err(error) => {
                        this.data = None;
                        this.error = error;
                    }
                }
                this.checking = false;
                cx.notify();
            });
        }));
    }

    /// What setting Codex up needs next: install or sign in.
    pub fn action(&self) -> Option<(Step, &'static str)> {
        setup_action(&self.error)
    }

    /// The copy's install or sign-in: its progress and Cancel while it
    /// runs, otherwise what Codex needs, then its output.
    fn setup_view(&self, c: &Colors) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .children(
                self.setup
                    .notice
                    .clone()
                    .map(|notice| div().text_body_sm().child(notice)),
            )
            .child(setup_steps(
                &self.host.setup,
                &self.setup,
                self.action(),
                self.checking,
                c,
            ))
    }
}

/// What setting Codex up needs next, from what its check said: install or
/// sign in.
pub fn setup_action(error: &str) -> Option<(Step, &'static str)> {
    match error {
        NOT_INSTALLED => Some((Step::Install, "Install Codex")),
        SIGN_IN => Some((Step::Login, "Sign in")),
        _ => None,
    }
}

/// The copy's install or sign-in, shared by the tabs and the provider
/// dialog: its progress and Cancel while it runs, otherwise the step
/// `action` names, then its output.
pub fn setup_steps(
    setup: &CodexSetup,
    state: &SetupState,
    action: Option<(Step, &'static str)>,
    checking: bool,
    c: &Colors,
) -> Div {
    let running = state.running;
    let start = |step: Step| {
        let setup = setup.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let setup = setup.clone();
            Runtime::handle(cx).spawn(async move { setup.run(step).await });
        }
    };
    let cancel = {
        let setup = setup.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let setup = setup.clone();
            Runtime::handle(cx).spawn(async move { setup.cancel().await });
        }
    };
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .when_some(running, |this, running| {
            this.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(space::S2)
                    .child(div().text_body_sm().child(match running {
                        Step::Login => "Waiting for Codex sign-in…",
                        Step::Install => "Installing Codex…",
                    }))
                    .child(Button::new("codex-setup-cancel", "Cancel").on_click(cancel)),
            )
        })
        .when_some(
            action.filter(|_| running.is_none() && !checking),
            |this, (step, label)| {
                this.child(
                    div().flex().child(
                        Button::new("codex-setup-action", label)
                            .prominent()
                            .on_click(start(step)),
                    ),
                )
            },
        )
        .when(
            matches!(action, Some((Step::Install, _))) && running.is_none(),
            |this| {
                this.child(note_with_code(
                    "Downloads Buzz's own copy of Codex (about 130 MB) from releases.openai.com and keeps it on the latest release. It does not change the",
                    "codex",
                    "in your terminal, and uses the same sign-in and settings.",
                    c,
                ))
            },
        )
        .when(!state.output.is_empty(), |this| {
            this.child(
                div()
                    .id("codex-setup-output")
                    .max_h(rems(15.))
                    .overflow_y_scroll()
                    .text_caption()
                    .text_mono()
                    .whitespace_normal()
                    .child(state.output.clone()),
            )
        })
}

impl Drop for CatalogCheck {
    fn drop(&mut self) {
        self.signal.abort("Codex check closed");
    }
}

/// The line that says where Codex stands.
pub fn status_line(check: &CatalogCheck) -> String {
    if check.checking {
        "Checking Codex…".into()
    } else if let Some(data) = &check.data {
        format!("Codex is ready ({}).", data.account)
    } else {
        "Codex needs attention.".into()
    }
}

const TEXTS: Texts = Texts {
    harness: "Codex",
    missing: "Codex has no history for this conversation on this computer. Its session may have been deleted.",
    turn_limit: TURN_LIMIT,
};

fn label(conversation: &SavedConversation) -> String {
    transcript_view::label(
        &conversation.channel_id,
        conversation.name.as_deref(),
        conversation.root.as_deref(),
    )
}

/// Codex on the agent's host, and this agent's conversations: the one its
/// profile was opened from set apart, then the recent ones.
pub struct CodexTab {
    pubkey: String,
    check: Entity<CatalogCheck>,
    runtime: CodexRuntime,
    sessions: Vec<SessionView>,
    spawn: Option<Spawner>,
    /// The conversation the agent's profile was opened from.
    conversation: Option<AgentConversation>,
    /// Its thread as a lookup in Codex's history found it; `None` while
    /// looking.
    found: Option<Found<SavedConversation>>,
    looking: bool,
    /// The transcript shown over the tab: its title and view.
    open: Option<(String, Entity<TranscriptView>)>,
    focus: FocusHandle,
    lookup_signal: Signal,
    _lookup: Option<Task<()>>,
    _task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl CodexTab {
    pub fn new(
        pubkey: String,
        conversation: Option<AgentConversation>,
        host: Host,
        runtime: CodexRuntime,
        cx: &mut Context<Self>,
    ) -> Self {
        let spawn = host.spawn.clone();
        let check = cx.new(|cx| CatalogCheck::new(host, cx));
        let mut changes = runtime.subscribe();
        let follow = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                if this
                    .update(cx, |this, cx| {
                        this.sessions = this.runtime.sessions(&this.pubkey);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            sessions: runtime.sessions(&pubkey),
            pubkey,
            _subscriptions: vec![cx.observe(&check, |_, _, cx| cx.notify())],
            check,
            runtime,
            spawn,
            conversation,
            found: None,
            looking: false,
            open: None,
            focus: cx.focus_handle(),
            lookup_signal: Signal::default(),
            _lookup: None,
            _task: follow,
        }
    }

    /// The agent's name, which its threads are named with.
    fn agent_name(&self, cx: &App) -> String {
        Agents2::global(cx)
            .and_then(|agents| {
                agents
                    .read(cx)
                    .find(&self.pubkey)
                    .map(|agent| agent.name.clone())
            })
            .unwrap_or_default()
    }

    /// The opened conversation's thread: this app's saved one for its
    /// thread, else one found for the thread or then its channel, saved
    /// here or started by another copy of the app. `None` while looking.
    fn here(
        &mut self,
        saved: &[SavedConversation],
        cx: &mut Context<Self>,
    ) -> Option<Found<SavedConversation>> {
        let places = places(self.conversation.as_ref()?);
        let bound = |place: &Place| {
            saved
                .iter()
                .find(|row| row.channel_id == place.channel_id && row.root == place.root)
                .cloned()
        };
        if let Some(exact) = bound(&places[0]) {
            return Some(Ok(Some(exact)));
        }
        if self.found.is_none() && !self.looking {
            self.look(
                places
                    .into_iter()
                    .map(|place| (bound(&place), place))
                    .collect(),
                cx,
            );
        }
        self.found.clone()
    }

    fn look(&mut self, places: Vec<(Option<SavedConversation>, Place)>, cx: &mut Context<Self>) {
        self.looking = true;
        let (runtime, spawn, pubkey, name, signal) = (
            self.runtime.clone(),
            self.spawn.clone(),
            self.pubkey.clone(),
            self.agent_name(cx),
            self.lookup_signal.clone(),
        );
        let search = Runtime::handle(cx).spawn(async move {
            for (bound, place) in places {
                if bound.is_some() {
                    return Ok(bound);
                }
                let Some(spawn) = &spawn else {
                    return Err("The agent's host is not connected".to_string());
                };
                let found = runtime
                    .find(spawn, (&pubkey, &name), &place, &signal)
                    .await?;
                if found.is_some() {
                    return Ok(found);
                }
            }
            Ok(None)
        });
        self._lookup = Some(cx.spawn(async move |this, cx| {
            let found = search
                .await
                .unwrap_or_else(|_| Err("The search stopped".into()));
            let _ = this.update(cx, |this, cx| {
                this.found = Some(found);
                cx.notify();
            });
        }));
    }

    /// Shows the conversation's transcript over the tab.
    fn show(
        &mut self,
        conversation: SavedConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spawn = self.spawn.clone();
        let lookup = Lookup {
            agent_pubkey: self.pubkey.clone(),
            agent_name: self.agent_name(cx),
            channel_id: conversation.channel_id.clone(),
            name: conversation.name.clone(),
            root: conversation.root.clone(),
            thread_id: Some(conversation.thread_id.clone()),
        };
        // Prompts name their senders as the agent's community knows them.
        let profiles = Agents2::global(cx)
            .and_then(|agents| agents.read(cx).relay(&self.pubkey))
            .map(|community| community.read(cx).snapshot.profiles.clone());
        let names: SharedNames = Arc::new(move |pubkey| {
            let profile = profiles.as_ref()?.get(pubkey)?;
            (!profile.name_is_fallback).then(|| profile.name.clone())
        });
        let reader: Reader = Arc::new(move |_, signal| {
            let (spawn, lookup, names) = (spawn.clone(), lookup.clone(), names.clone());
            Box::pin(async move {
                let Some(spawn) = spawn else {
                    return Err("The agent's host is not connected".into());
                };
                let read = read_transcript(&spawn, &lookup, &signal, &*names).await?;
                Ok(read.map(|read| Loaded {
                    transcript: read.transcript,
                    more: read.more,
                    working: Some(read.working),
                }))
            })
        });
        let pubkey = self.pubkey.clone();
        let view = cx.new(|cx| TranscriptView::new(pubkey, TEXTS, reader, false, cx));
        self.open = Some((label(&conversation), view));
        window.focus(&self.focus, cx);
        cx.notify();
    }
}

/// A conversation's status detail, scrolled when long.
fn detail(key: &str, text: &str) -> Stateful<Div> {
    div()
        .id(SharedString::from(format!("codex-detail-{key}")))
        .max_h(rems(12.))
        .overflow_y_scroll()
        .text_caption()
        .text_mono()
        .whitespace_normal()
        .child(text.to_string())
}

impl Render for CodexTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((title, view)) = &self.open {
            let weak = cx.entity().downgrade();
            let back: Back = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.open = None;
                    cx.notify();
                });
            });
            return div().child(opened::subview(
                title.clone(),
                &self.focus,
                back,
                view.clone(),
            ));
        }
        let c = *cx.colors();
        // Read on each render: bindings change as turns start, which also
        // changes the session list this tab follows.
        let saved = self.runtime.conversations(&self.pubkey);
        let here = self.here(&saved, cx);
        let here_key = match &here {
            Some(Ok(Some(session))) => Some(session.key.clone()),
            _ => None,
        };
        let card = self.conversation.as_ref().map(|conversation| {
            let shown = here.clone().map(|found| {
                found.map(|session| {
                    session.map(|session| CardSession {
                        status: self
                            .sessions
                            .iter()
                            .find(|view| view.key == session.key)
                            .map(|view| view.status.clone()),
                        channel_id: session.channel_id,
                        name: session.name,
                        root: session.root,
                        at: session.at,
                    })
                })
            });
            let (weak, session) = (cx.entity().downgrade(), here.clone());
            opened::card(
                "codex-here",
                conversation,
                "Codex",
                shown,
                move |window, cx| {
                    if let Some(Ok(Some(session))) = session.clone() {
                        let _ = weak.update(cx, |this, cx| this.show(session, window, cx));
                    }
                },
                &c,
            )
        });
        let check = self.check.read(cx);
        let checking = check.checking;
        // Work that failed before Codex started a thread has no binding.
        let unbound: Vec<&SessionView> = self
            .sessions
            .iter()
            .filter(|view| !saved.iter().any(|row| row.key == view.key))
            .collect();
        let recent: Vec<SavedConversation> = saved
            .into_iter()
            .filter(|row| Some(&row.key) != here_key.as_ref())
            .take(CONVERSATION_LIMIT)
            .collect();
        let conversations = if recent.is_empty() && unbound.is_empty() {
            description_text(
                "Mention this agent in a channel to start a conversation.",
                &c,
            )
        } else {
            let unbound = unbound.into_iter().map(|session| {
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S1)
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .child(div().text_mono().child(session.key.clone()))
                            .child(format!(" · {}", session.status)),
                    )
                    .when(!session.detail.is_empty(), |this| {
                        this.child(detail(&session.key, &session.detail))
                    })
            });
            let bound = recent.into_iter().map(|row| {
                let view = self.sessions.iter().find(|view| view.key == row.key);
                let line: String = std::iter::once(label(&row))
                    .chain(view.map(|view| view.status.clone()))
                    .chain(row.at.map(date_time))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let id = SharedString::from(format!("codex-transcript-{}", row.key));
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S2)
                    .min_w_0()
                    .child(div().child(line))
                    .when_some(view.filter(|view| !view.detail.is_empty()), |this, view| {
                        this.child(detail(&view.key, &view.detail))
                    })
                    .child(
                        div()
                            .flex()
                            .child(Button::new(id, "View transcript").ghost().on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.show(row.clone(), window, cx)
                                }),
                            )),
                    )
            });
            div()
                .flex()
                .flex_col()
                .gap(space::S4)
                .children(unbound.map(IntoElement::into_any_element))
                .children(bound.map(IntoElement::into_any_element))
        };
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .text_body()
            .child(div().child(status_line(check)))
            .when(!check.error.is_empty(), |this| {
                this.child(div().child(check.error.clone()))
            })
            .child(check.setup_view(&c))
            .child(
                div().flex().child(
                    Button::new("codex-check", "Check again")
                        .disabled(checking)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.check.update(cx, |check, cx| check.retry(cx));
                        })),
                ),
            )
            .child(description_text(
                "Uses your Codex sign-in and settings.",
                &c,
            ))
            .children(card)
            .child(opened::heading("Recent conversations"))
            .child(conversations)
            .child(description_text(
                "Only your messages trigger this agent. New mentions steer active work and continue the same session when idle. Event watches work for your messages; timers are not supported yet.",
                &c,
            ))
    }
}

/// Which select is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picker {
    Model,
    Effort,
    Scope,
}

/// The catalog's models, for the picker; it adds Codex's own default.
pub fn model_choices(models: &[Model]) -> Vec<ModelChoice> {
    models
        .iter()
        .map(|model| ModelChoice {
            id: model.model.clone(),
            label: model.display_name.clone(),
            description: Some(model.description.clone()).filter(|text| !text.is_empty()),
        })
        .collect()
}

/// The Thinking select for the selected model: Codex's default, named
/// after the model's default effort, a saved effort it no longer offers,
/// then every effort it offers.
pub fn effort_options(draft: &Config, selected: Option<&Model>) -> Options {
    let efforts = selected.map_or(&[][..], |model| &model.supported_reasoning_efforts);
    let default = match selected
        .map(|model| model.default_reasoning_effort.as_str())
        .filter(|effort| !effort.is_empty())
    {
        Some(effort) => format!("Codex default ({})", effort_name(effort)),
        None => "Codex default".into(),
    };
    let mut options = vec![(String::new(), default)];
    if !draft.effort.is_empty()
        && !efforts
            .iter()
            .any(|effort| effort.reasoning_effort == draft.effort)
    {
        options.push((
            draft.effort.clone(),
            format!("{} (unavailable)", effort_name(&draft.effort)),
        ));
    }
    options.extend(efforts.iter().map(|effort| {
        (
            effort.reasoning_effort.clone(),
            effort_name(&effort.reasoning_effort),
        )
    }));
    options
}

/// The model the draft runs: its own, or Codex's default.
pub fn selected_model<'a>(draft: &Config, models: &'a [Model]) -> Option<&'a Model> {
    models
        .iter()
        .find(|model| model.model == draft.model)
        .or_else(|| {
            draft
                .model
                .is_empty()
                .then(|| models.iter().find(|model| model.is_default))
                .flatten()
        })
}

/// Checks a draft before it is saved.
pub fn validate(draft: &Config) -> Result<Config, String> {
    let settings = Config::read(&draft.value());
    if !valid_workspace(&settings.workspace) {
        return Err("Enter an absolute or ~/… workspace path.".into());
    }
    Ok(settings)
}

/// What the agent runs with.
pub struct SettingsTab {
    view: AgentView,
    check: Entity<CatalogCheck>,
    /// The choices made with selects; the text fields hold the rest.
    draft: Config,
    /// The saved settings the draft started from.
    saved: Config,
    workspace: Entity<InputState>,
    instructions: Entity<TextareaState>,
    open: Option<Picker>,
    saving: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsTab {
    pub fn new(view: AgentView, host: Host, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = Config::read(&saved_config(&view, cx));
        let check = cx.new(|cx| CatalogCheck::new(host, cx));
        let workspace = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(DEFAULT_WORKSPACE)
                .default_value(saved.workspace.clone())
        });
        let instructions = instructions_state(&saved.instructions, window, cx);
        let mut subscriptions = vec![
            cx.observe(&check, |_, _, cx| cx.notify()),
            cx.subscribe(&workspace, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&instructions, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        // Saved settings that change, here or elsewhere, replace the draft.
        if let Some(agents) = Agents2::global(cx) {
            subscriptions.push(cx.observe_in(&agents, window, |this, _, window, cx| {
                let saved = this.saved(cx);
                if saved != this.saved {
                    this.reset(saved, window, cx);
                }
            }));
        }
        Self {
            view,
            check,
            draft: saved.clone(),
            saved,
            workspace,
            instructions,
            open: None,
            saving: false,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn saved(&self, cx: &App) -> Config {
        Config::read(&saved_config(&self.view, cx))
    }

    fn reset(&mut self, saved: Config, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |state, cx| {
            state.set_value(saved.workspace.clone(), window, cx)
        });
        self.instructions.update(cx, |state, cx| {
            state.set_value(saved.instructions.clone(), window, cx)
        });
        self.draft = saved.clone();
        self.saved = saved;
        cx.notify();
    }

    fn current(&self, cx: &App) -> Config {
        Config {
            workspace: self.workspace.read(cx).value().to_string(),
            instructions: self.instructions.read(cx).value().to_string(),
            ..self.draft.clone()
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let settings = match validate(&self.current(cx)) {
            Ok(settings) => settings,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.saving = true;
        let save = self.view.save(settings.value(), cx);
        cx.spawn(async move |this, cx| {
            let result = save.await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                this.error = result.err();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Picking for SettingsTab {
    type Picker = Picker;

    fn open(&mut self) -> &mut Option<Picker> {
        &mut self.open
    }

    fn pick(&mut self, picker: Picker, value: String, cx: &mut Context<Self>) {
        match picker {
            // A model's choice also resets the effort to that model's
            // default.
            Picker::Model => {
                let effort = self
                    .check
                    .read(cx)
                    .data
                    .as_ref()
                    .and_then(|data| data.models.iter().find(|model| model.model == value))
                    .map(|model| model.default_reasoning_effort.clone())
                    .unwrap_or_default();
                self.draft.model = value;
                self.draft.effort = effort;
            }
            Picker::Effort => self.draft.effort = value,
            Picker::Scope => self.draft.scope = value,
        }
    }
}

impl Render for SettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let current = self.current(cx);
        let changed = current != self.saved(cx);
        let check = self.check.read(cx);
        let (checking, catalog_error) = (check.checking, check.error.clone());
        let setup = check.setup_view(&c);
        let models = check.data.as_ref().map(|data| data.models.clone());
        let selected = models
            .as_deref()
            .and_then(|models| selected_model(&self.draft, models))
            .cloned();
        let choices = model_choices(models.as_deref().unwrap_or_default());
        // The default rows follow the device defaults (`providerDefaultLabel`).
        let default_label = super::provider::default_model_label(cx);
        let mut effort_choices = effort_options(&self.draft, selected.as_ref());
        if let Some((_, label)) = effort_choices.first_mut() {
            *label = super::provider::default_effort_label_for(&self.draft.model, cx);
        }
        let scope = if self.draft.scope == "channel" {
            "channel"
        } else {
            "thread"
        };
        let scopes = vec![
            ("thread".to_string(), "One session per thread".to_string()),
            ("channel".to_string(), "One session per channel".to_string()),
        ];
        let saving = self.saving;
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .children(self.view.type_control(changed, window, cx))
            .when(!catalog_error.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(space::S2)
                        .child(catalog_error)
                        .child(setup)
                        .child(
                            Button::new("codex-settings-check", "Check again")
                                .ghost()
                                .disabled(checking)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.check.update(cx, |check, cx| check.retry(cx));
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(space::S4)
                    .child(
                        div().flex_1().min_w(rems(14.)).child(model_picker(
                            self.open,
                            Picker::Model,
                            "codex-model",
                            &choices,
                            false,
                            &self.draft.model,
                            &default_label,
                            Some("Following the provider's model default"),
                            checking,
                            saving,
                            window,
                            cx,
                        )),
                    )
                    .child(div().flex_1().min_w(rems(14.)).child(field(
                        "Thinking",
                        choice(
                            self.open,
                            Picker::Effort,
                            "codex-effort",
                            &self.draft.effort,
                            effort_choices,
                            checking || saving || selected.is_none(),
                            window,
                            cx,
                        ),
                        None,
                        &c,
                    ))),
            )
            .child(field(
                "Workspace",
                TextField::new(&self.workspace)
                    .surface(Surface::Floating)
                    .disabled(saving),
                Some(
                    "Codex starts here with full access to your files and network. Buzz tools run inside the app. Inherited MCP/Apps/plugin tools are disabled."
                        .into(),
                ),
                &c,
            ))
            .child(field(
                "Sessions",
                choice(
                    self.open,
                    Picker::Scope,
                    "codex-scope",
                    scope,
                    scopes,
                    saving,
                    window,
                    cx,
                ),
                Some("DMs always share one conversation.".into()),
                &c,
            ))
            .child(instructions_field(
                &self.instructions,
                saving,
                "Buzz guidance supplements Codex’s native instructions. Saved changes apply to the next turn. A workspace change starts a fresh session.",
                &c,
            ))
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .child(
                div().flex().child(
                    Button::new("codex-save", if saving { "Saving…" } else { "Save" })
                        .prominent()
                        .loading(saving)
                        .disabled(saving || !(changed || self.view.pending()))
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
            )
    }
}

#[cfg(test)]
mod tests;
