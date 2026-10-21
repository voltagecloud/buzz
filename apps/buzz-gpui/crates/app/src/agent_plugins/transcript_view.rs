//! One conversation's session transcript in an agent type's tab
//! (buzz-app's `TranscriptView` in the Claude and Codex tabs): read on
//! open and on Refresh, shown with the Agent Activity transcript. A
//! refresh keeps the last result in place until the new one arrives. The
//! tab shows it over itself, with the way back.

use crate::agent_activity::transcript_model::Transcript;
use crate::agent_activity::turns::TurnState;
use crate::agents2::Agents2;
use crate::buzz_mcp::app_client::Signal;
use crate::community::Community;
use crate::runtime::Runtime;
use crate::ui::agent_activity::transcript::{TranscriptHost, timeline};
use buzz_ui::{ActiveTheme, Button, TypeStyled, tokens::space};
use futures::future::BoxFuture;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// What a read found: its latest turns, whether there are earlier ones,
/// and whether the session is working now, when the read can tell.
pub struct Loaded {
    pub transcript: Transcript,
    pub more: bool,
    pub working: Option<bool>,
}

/// Leaves the transcript.
pub type Back = Rc<dyn Fn(&mut Window, &mut App)>;

/// Reads the transcript; `true` when the session is known to be
/// working. `None` when the host has no transcript for it.
pub type Reader =
    Arc<dyn Fn(bool, Signal) -> BoxFuture<'static, Result<Option<Loaded>, String>> + Send + Sync>;

/// The type's words for its sessions.
#[derive(Clone, Copy)]
pub struct Texts {
    /// The type's name for its sessions, e.g. Codex.
    pub harness: &'static str,
    /// Why there is nothing to show.
    pub missing: &'static str,
    /// The most turns a read shows.
    pub turn_limit: usize,
}

/// A conversation as its tab lists it: `#name` or the start of the
/// channel id, then its thread or "channel".
pub fn label(channel_id: &str, name: Option<&str>, root: Option<&str>) -> String {
    let place = match name {
        Some(name) => format!("#{name}"),
        None => channel_id.chars().take(8).collect(),
    };
    match root {
        Some(root) => format!("{place} · thread {}", short(root)),
        None => format!("{place} · channel"),
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

enum Read {
    Ready(Arc<Transcript>, bool),
    Missing,
    Error(String),
}

pub struct TranscriptView {
    pubkey: String,
    texts: Texts,
    reader: Reader,
    read: Option<Read>,
    loading: bool,
    /// The session is working: given by the tab, or found by the read.
    working: bool,
    expanded: HashSet<String>,
    signal: Signal,
    _task: Option<Task<()>>,
}

impl TranscriptView {
    pub fn new(
        pubkey: String,
        texts: Texts,
        reader: Reader,
        working: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            pubkey,
            texts,
            reader,
            read: None,
            loading: true,
            working,
            expanded: HashSet::new(),
            signal: Signal::default(),
            _task: None,
        };
        this.refresh(cx);
        this
    }

    /// A turn ending rereads, so its last steps show without a Refresh.
    pub fn set_working(&mut self, working: bool, cx: &mut Context<Self>) {
        if working != self.working {
            self.working = working;
            self.refresh(cx);
        }
    }

    /// Reads again, abandoning a read in flight.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.signal.abort("Transcript read replaced");
        self.signal = Signal::default();
        self.loading = true;
        cx.notify();
        let signal = self.signal.clone();
        let read = Runtime::handle(cx).spawn((self.reader)(self.working, signal.clone()));
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = read
                .await
                .unwrap_or_else(|_| Err("The read stopped".into()));
            let _ = this.update(cx, |this, cx| {
                if signal.aborted() {
                    return;
                }
                this.read = Some(match result {
                    Ok(Some(loaded)) => {
                        if let Some(working) = loaded.working {
                            this.working = working;
                        }
                        Read::Ready(Arc::new(loaded.transcript), loaded.more)
                    }
                    Ok(None) => Read::Missing,
                    Err(error) => Read::Error(error),
                });
                this.loading = false;
                cx.notify();
            });
        }));
    }
}

impl Drop for TranscriptView {
    fn drop(&mut self) {
        self.signal.abort("Transcript closed");
    }
}

impl TranscriptHost for TranscriptView {
    fn expanded(&mut self) -> &mut HashSet<String> {
        &mut self.expanded
    }
    fn community(&self, cx: &App) -> Option<Entity<Community>> {
        Agents2::global(cx)?.read(cx).relay(&self.pubkey)
    }
}

impl Render for TranscriptView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let harness = self.texts.harness;
        let ready = match &self.read {
            Some(Read::Ready(transcript, more)) => Some((transcript.clone(), *more)),
            _ => None,
        };
        let content: AnyElement = match (&self.read, ready) {
            (None, _) => div()
                .child(format!("Reading the {harness} session…"))
                .into_any_element(),
            (Some(Read::Missing), _) => div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(self.texts.missing)
                .into_any_element(),
            (Some(Read::Error(error)), _) => div()
                .child(format!("Could not read the {harness} session: {error}"))
                .into_any_element(),
            (Some(Read::Ready(..)), None) => div().into_any_element(),
            (Some(Read::Ready(..)), Some((transcript, more))) => {
                let before = more
                    .then(|| {
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child(format!(
                                "Showing the latest {} turns.",
                                self.texts.turn_limit
                            ))
                            .into_any_element()
                    })
                    .into_iter()
                    .collect();
                // Turns not ended are working while the session is.
                let states: HashMap<String, TurnState> = if self.working {
                    transcript
                        .turns
                        .iter()
                        .map(|turn| (turn.turn_id.clone(), TurnState::Working))
                        .collect()
                } else {
                    HashMap::new()
                };
                let name = Agents2::global(cx)
                    .and_then(|agents| {
                        agents
                            .read(cx)
                            .find(&self.pubkey)
                            .map(|agent| agent.name.clone())
                    })
                    .unwrap_or_default();
                timeline(self, &transcript, &name, &states, before, cx)
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .min_w_0()
            .text_body()
            .child(
                div().flex().child(
                    Button::new("transcript-refresh", "Refresh")
                        .ghost()
                        .loading(self.loading)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                ),
            )
            .child(content)
    }
}
