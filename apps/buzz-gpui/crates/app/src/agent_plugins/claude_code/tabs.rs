//! The type's tabs (buzz-app's `tabs.ts`): Claude, with Claude's
//! status on the agent's host, its setup and the agent's conversations,
//! each with its session's transcript; and Settings, what the agent runs
//! with, under the app's provider select.

use super::provider::SetupControls;
use super::runtime::{ClaudeRuntime, Config, SavedConversation};
use super::sessions::SessionView;
use super::setup::ClaudeSetup;
use super::transcript::{TURN_LIMIT, read_transcript};
use crate::agent_plugins::opened::{self, CONVERSATION_LIMIT, CardSession, Found, Place, places};
use crate::agent_plugins::process::Spawner;
use crate::agent_plugins::tabs::{
    CUSTOM_MODEL, Picking, choice, custom_model_dialog, custom_model_input, description_text,
    field, instructions_field, instructions_state, listed_model, model_picker, options,
    saved_config,
};
use crate::agent_plugins::transcript_view::{self, Back, Loaded, Reader, Texts, TranscriptView};
use crate::agents2::{AgentConversation, AgentView, Agents2};
use crate::buzz_mcp::app_client::Signal;
use crate::runtime::Runtime;
use crate::ui::agent_activity::panel::date_time;
use buzz_ui::{ActiveTheme, Button, Surface, TextField, TypeStyled, tokens::space};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::*;
use std::rc::Rc;
use std::sync::Arc;

const SCOPES: [(&str, &str); 2] = [
    ("thread", "One session per thread"),
    ("channel", "One session per channel"),
];

const TEXTS: Texts = Texts {
    harness: "Claude",
    missing: "Claude has no transcript for this conversation on this computer. It may have been deleted, or the session ran in a different workspace.",
    turn_limit: TURN_LIMIT,
};

fn label(conversation: &SavedConversation) -> String {
    transcript_view::label(
        &conversation.channel_id,
        conversation.name.as_deref(),
        conversation.root.as_deref(),
    )
}

/// Claude on the agent's host, and this agent's conversations: the one
/// its profile was opened from set apart, then the recent ones.
pub struct ClaudeTab {
    pubkey: String,
    runtime: ClaudeRuntime,
    sessions: Vec<SessionView>,
    /// The conversation the agent's profile was opened from.
    conversation: Option<AgentConversation>,
    /// Its session as a lookup in the agent's Claude history found it;
    /// `None` while looking.
    found: Option<Found<SavedConversation>>,
    looking: bool,
    /// The transcript shown over the tab: its key, title and view.
    open: Option<(String, String, Entity<TranscriptView>)>,
    focus: FocusHandle,
    _lookup: Option<Task<()>>,
    lookup_signal: Signal,
    setup: Entity<SetupControls>,
    _task: Task<()>,
}

impl ClaudeTab {
    pub fn new(
        pubkey: String,
        conversation: Option<AgentConversation>,
        setup: ClaudeSetup,
        runtime: ClaudeRuntime,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut runtime_changes = runtime.subscribe();
        let follow_sessions = cx.spawn(async move |this, cx| {
            while runtime_changes.changed().await.is_ok() {
                if this
                    .update(cx, |this, cx| {
                        this.sessions = this.runtime.sessions(&this.pubkey);
                        if let Some((key, _, view)) = &this.open {
                            let working = this.busy(key).unwrap_or(false);
                            view.update(cx, |view, cx| view.set_working(working, cx));
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let check = setup.clone();
        Runtime::handle(cx).spawn(async move { check.check().await });
        Self {
            sessions: runtime.sessions(&pubkey),
            setup: cx.new(|cx| SetupControls::new(setup, true, window, cx)),
            pubkey,
            runtime,
            conversation,
            found: None,
            looking: false,
            open: None,
            focus: cx.focus_handle(),
            _lookup: None,
            lookup_signal: Signal::default(),
            _task: follow_sessions,
        }
    }

    /// Whether the conversation's session is working; `None` when none
    /// is running.
    fn busy(&self, key: &str) -> Option<bool> {
        self.sessions
            .iter()
            .find(|session| session.key == key)
            .map(|session| session.busy)
    }

    /// The agent's host and workspace, where its sessions are.
    fn place(&self, cx: &App) -> (Option<Spawner>, String) {
        let agents = Agents2::global(cx);
        let spawn = agents.as_ref().and_then(|agents| {
            let host = agents.read(cx).host_of(&self.pubkey)?;
            Some(host.read(cx).spawner(super::PROCESS_OWNER))
        });
        let workspace = agents
            .and_then(|agents| {
                let agent = agents.read(cx).find(&self.pubkey)?;
                Some(Config::read(&agent.config).workspace)
            })
            .unwrap_or_else(|| Config::default().workspace);
        (spawn, workspace)
    }

    /// The opened conversation's session: this app's saved one for its
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
        let (spawn, workspace) = self.place(cx);
        let (runtime, pubkey, signal) = (
            self.runtime.clone(),
            self.pubkey.clone(),
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
                    .find(spawn, &pubkey, &place, &workspace, &signal)
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
        let (spawn, workspace) = self.place(cx);
        let session = conversation.conversation(&self.pubkey);
        let reader: Reader = Arc::new(move |working, signal| {
            let (spawn, workspace, session) = (spawn.clone(), workspace.clone(), session.clone());
            Box::pin(async move {
                let Some(spawn) = spawn else {
                    return Err("The agent's host is not connected".into());
                };
                let read = read_transcript(&spawn, &workspace, &session, working, &signal).await?;
                Ok(read.map(|read| Loaded {
                    transcript: read.transcript,
                    more: read.more,
                    working: None,
                }))
            })
        });
        let working = self.busy(&conversation.key).unwrap_or(false);
        let pubkey = self.pubkey.clone();
        let view = cx.new(|cx| TranscriptView::new(pubkey, TEXTS, reader, working, cx));
        self.open = Some((conversation.key.clone(), label(&conversation), view));
        window.focus(&self.focus, cx);
        cx.notify();
    }
}

impl Render for ClaudeTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((_, title, view)) = &self.open {
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
        let conversations = self.runtime.conversations(&self.pubkey);
        let here = self.here(&conversations, cx);
        let here_key = match &here {
            Some(Ok(Some(session))) => Some(session.key.clone()),
            _ => None,
        };
        let setup = self.setup.clone();
        let card = self.conversation.as_ref().map(|conversation| {
            let shown = here.clone().map(|found| {
                found.map(|session| {
                    session.map(|session| CardSession {
                        status: self
                            .busy(&session.key)
                            .map(|busy| if busy { "Working" } else { "Idle" }.to_string()),
                        channel_id: session.channel_id,
                        name: session.name,
                        root: session.root,
                        at: Some(session.at),
                    })
                })
            });
            let (weak, session) = (cx.entity().downgrade(), here.clone());
            opened::card(
                "claude-here",
                conversation,
                "Claude",
                shown,
                move |window, cx| {
                    if let Some(Ok(Some(session))) = session.clone() {
                        let _ = weak.update(cx, |this, cx| this.show(session, window, cx));
                    }
                },
                &c,
            )
        });
        let recent: Vec<SavedConversation> = conversations
            .into_iter()
            .filter(|row| Some(&row.key) != here_key.as_ref())
            .take(CONVERSATION_LIMIT)
            .collect();
        let list = if recent.is_empty() {
            description_text(
                "None. A conversation's session starts when the agent is mentioned.",
                &c,
            )
        } else {
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .children(recent.into_iter().map(|conversation| {
                    let activity = self
                        .busy(&conversation.key)
                        .map(|busy| if busy { "Working" } else { "Idle" }.to_string());
                    let detail: Vec<String> = activity
                        .into_iter()
                        .chain([date_time(conversation.at)])
                        .collect();
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(space::S2)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(div().truncate().child(label(&conversation)))
                                .child(
                                    div()
                                        .text_body_sm()
                                        .text_color(c.text_subtle)
                                        .child(detail.join(" · ")),
                                ),
                        )
                        .child(
                            Button::new(
                                SharedString::from(format!(
                                    "claude-transcript-{}",
                                    conversation.key
                                )),
                                "View transcript",
                            )
                            .ghost()
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.show(conversation.clone(), window, cx)
                                },
                            )),
                        )
                }))
        };
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .text_body()
            .child(setup)
            .children(card)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S1)
                    .child(opened::heading("Recent conversations"))
                    .child(list),
            )
    }
}

/// Which select is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picker {
    Model,
    Scope,
}

/// What the agent runs with. Saving restarts idle sessions with the change.
pub struct SettingsTab {
    view: AgentView,
    /// The choices made with selects; the text fields hold the rest.
    draft: Config,
    /// The saved settings the draft started from.
    saved: Config,
    workspace: Entity<InputState>,
    instructions: Entity<TextareaState>,
    open: Option<Picker>,
    /// The custom-model form, once asked for: its initial id until render
    /// makes its field.
    custom_asked: Option<String>,
    custom: Option<Entity<InputState>>,
    saving: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsTab {
    pub fn new(view: AgentView, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = Config::read(&saved_config(&view, cx));
        let workspace =
            cx.new(|cx| InputState::new(window, cx).default_value(saved.workspace.clone()));
        let instructions = instructions_state(&saved.instructions, window, cx);
        let mut subscriptions = vec![
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
            draft: saved.clone(),
            saved,
            workspace,
            instructions,
            open: None,
            custom_asked: None,
            custom: None,
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
        let mut config = self.current(cx);
        if config.workspace.trim().is_empty() {
            config.workspace = "~/.buzz".into();
        } else {
            config.workspace = config.workspace.trim().to_string();
        }
        self.saving = true;
        self.error = None;
        let save = self.view.save(config.value(), cx);
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
            Picker::Model if value == CUSTOM_MODEL => {
                let models = create_models(cx).models;
                let listed = listed_model(&models, &self.draft.model);
                self.custom_asked = Some(if listed {
                    String::new()
                } else {
                    self.draft.model.clone()
                });
            }
            Picker::Model => self.draft.model = value,
            Picker::Scope => self.draft.scope = value,
        }
    }
}

/// Claude's models and its default row, which follows the device default.
fn create_models(cx: &App) -> crate::agents2::CreateModels {
    super::ClaudeCode::global(cx)
        .map(|state| state.read(cx).provider.read(cx).create_models())
        .unwrap_or_default()
}

impl Render for SettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        if let Some(initial) = self.custom_asked.take() {
            self.custom = Some(custom_model_input(&initial, window, cx));
        }
        let current = self.current(cx);
        let changed = current != self.saved(cx);
        let scope = if self.draft.scope == "channel" {
            "channel"
        } else {
            "thread"
        };
        let models = create_models(cx);
        let custom = self.custom.as_ref().map(|input| {
            custom_model_dialog(
                input,
                window,
                cx,
                |this: &mut Self, model, _| {
                    this.draft.model = model;
                    this.custom = None;
                },
                |this: &mut Self| this.custom = None,
            )
        });
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .children(self.view.type_control(changed, window, cx))
            .child(model_picker(
                self.open,
                Picker::Model,
                "claude-code-model",
                &models.models,
                models.allow_custom,
                &self.draft.model,
                &models.default_label,
                models.default_description.as_deref(),
                false,
                self.saving,
                window,
                cx,
            ))
            .child(field(
                "Workspace",
                TextField::new(&self.workspace)
                    .surface(Surface::Floating)
                    .disabled(self.saving),
                Some("The folder Claude works in. Use ~/ for your home folder.".into()),
                &c,
            ))
            .child(field(
                "Sessions",
                choice(
                    self.open,
                    Picker::Scope,
                    "claude-code-scope",
                    scope,
                    options(&SCOPES),
                    self.saving,
                    window,
                    cx,
                ),
                None,
                &c,
            ))
            .child(instructions_field(
                &self.instructions,
                self.saving,
                "Added to every session's system prompt, after Buzz's own.",
                &c,
            ))
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .child(
                div().flex().child(
                    Button::new("claude-code-save", "Save")
                        .prominent()
                        .loading(self.saving)
                        .disabled(!(changed || self.view.pending()) || self.saving)
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
            )
            .children(custom)
    }
}
