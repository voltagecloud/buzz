//! "Channel Canvas" (`ChannelCanvasDialog.tsx`): the channel's shared
//! Markdown, opened from Channel actions → "View canvas".
//!
//! The draft and the revision it started from are saved locally under
//! `canvas-draft-v1:<channel>` once typing pauses, and when the dialog
//! closes, so closing the dialog or the app keeps them. Saves name that base revision; when the saved Canvas
//! moved on, the draft is kept and only an explicit "Discard and reload"
//! replaces it. The History tab ([`history`]) lists saved revisions and
//! restores one as a new revision.

mod history;

use super::modal::{Dismissed, error};
use super::overlay::{self, Dismiss};
use crate::community::{AppState, Community};
use buzz_relay_client::canvas::{CanvasError, Revision};
use buzz_relay_client::{EventId, Partition, Store};
use buzz_ui::{ActiveTheme, Button, Dialog, Surface, Tab, TabVariant, Tabs, TextArea, TypeStyled};
use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::*;
use history::History;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

const INTRO: &str = "Shared Markdown for this channel. Saves and restores check the loaded revision on supporting relays.";
const STALE: &str = "Canvas changed since this draft began. Your draft is kept. Copy any edits you want to keep before reloading the saved Canvas.";
const RESTORED_DIRTY: &str = "Canvas was restored. Your unsaved draft is kept. Copy any edits you want to keep before reloading the saved Canvas.";

/// The locally saved draft (`{content, base}`), as buzz-app stores it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedDraft {
    content: String,
    base: Option<String>,
}

fn draft_key(channel: &str) -> String {
    format!("canvas-draft-v1:{channel}")
}

/// A saved draft, or `None` when absent or malformed.
fn parse_draft(json: &str) -> Option<SavedDraft> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let content = value.get("content")?.as_str()?.to_owned();
    let base = match value.get("base")? {
        serde_json::Value::Null => None,
        serde_json::Value::String(base) => Some(base.clone()),
        _ => return None,
    };
    Some(SavedDraft { content, base })
}

/// How long typing may pause before a draft is written.
const DRAFT_DELAY: Duration = Duration::from_millis(300);

struct PendingDraft<T> {
    store: Store,
    partition: Partition,
    key: String,
    draft: Option<T>,
}

impl<T: Serialize> PendingDraft<T> {
    fn write(self) {
        let json = self
            .draft
            .map(|draft| serde_json::to_string(&draft).expect("drafts serialize"));
        let written = self
            .store
            .set_view_state(&self.partition, &self.key, json.as_deref());
        if let Err(error) = written {
            tracing::warn!(%error, key = %self.key, "could not save a draft");
        }
    }
}

/// Writes a locally saved draft once typing pauses instead of on every
/// keystroke. What is still pending is written on [`flush`] and when the
/// writer drops, so closing keeps the last edit.
///
/// [`flush`]: DraftWriter::flush
pub(crate) struct DraftWriter<T: Serialize + 'static> {
    pending: Rc<RefCell<Option<PendingDraft<T>>>>,
    _timer: Option<Task<()>>,
}

impl<T: Serialize + 'static> Default for DraftWriter<T> {
    fn default() -> Self {
        Self {
            pending: Rc::default(),
            _timer: None,
        }
    }
}

impl<T: Serialize + 'static> DraftWriter<T> {
    /// Saves `draft` under `key` (`None` removes it): soon when `later`,
    /// otherwise now. Either replaces a write still pending.
    pub(crate) fn save(
        &mut self,
        partition: Partition,
        key: String,
        draft: Option<T>,
        later: bool,
        cx: &App,
    ) {
        let draft = PendingDraft {
            store: cx.global::<AppState>().store.clone(),
            partition,
            key,
            draft,
        };
        if !later {
            self.pending.borrow_mut().take();
            self._timer = None;
            draft.write();
            return;
        }
        *self.pending.borrow_mut() = Some(draft);
        let pending = self.pending.clone();
        let executor = cx.background_executor().clone();
        self._timer = Some(cx.spawn(async move |_| {
            executor.timer(DRAFT_DELAY).await;
            let draft = pending.borrow_mut().take();
            if let Some(draft) = draft {
                draft.write();
            }
        }));
    }

    /// Writes what is pending now.
    pub(crate) fn flush(&mut self) {
        self._timer = None;
        let draft = self.pending.borrow_mut().take();
        if let Some(draft) = draft {
            draft.write();
        }
    }
}

impl<T: Serialize + 'static> Drop for DraftWriter<T> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// `String(error)` of a JavaScript `Error`, as buzz-app shows read
/// failures.
pub(crate) fn js_error(error: &CanvasError) -> String {
    match error {
        CanvasError::Conflict => format!("CanvasConflictError: {error}"),
        CanvasError::Failed(message) => format!("Error: {message}"),
    }
}

/// The editor's view of the Canvas: what the dialog enables follows from
/// these alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct EditorState {
    head: Option<Revision>,
    base: Option<String>,
    draft: String,
    loaded: bool,
}

impl EditorState {
    fn head_id(&self) -> Option<String> {
        self.head.as_ref().map(|head| head.id.to_hex())
    }

    /// The draft is not the saved Canvas, or started from another one.
    fn dirty(&self) -> bool {
        self.head_id() != self.base
            || self.draft != self.head.as_ref().map_or("", |head| head.content.as_str())
    }

    /// "Save Canvas" needs the current head to be the draft's base.
    fn can_save(&self, available: bool) -> bool {
        self.loaded && available && self.head_id() == self.base
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Edit,
    History,
}

pub struct CanvasDialog {
    community: Entity<Community>,
    channel: String,
    view: View,
    /// The draft found when the dialog opened.
    saved: Option<SavedDraft>,
    editor: Entity<TextareaState>,
    state: EditorState,
    available: bool,
    busy: bool,
    error: Option<SharedString>,
    confirm_reload: bool,
    /// Bumped by each load, save and restore; older results are dropped.
    operation: u64,
    drafts: DraftWriter<SavedDraft>,
    history: Option<Entity<History>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Dismissed> for CanvasDialog {}

impl CanvasDialog {
    pub fn new(
        community: Entity<Community>,
        channel: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let partition = community.read(cx).partition();
        let saved = cx
            .global::<AppState>()
            .store
            .view_state(&partition, &draft_key(&channel))
            .ok()
            .flatten()
            .and_then(|json| parse_draft(&json));
        let draft = saved
            .as_ref()
            .map(|s| s.content.clone())
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).auto_grow(16, 16);
            state.set_value(draft.clone(), window, cx);
            state
        });
        let subscription = cx.subscribe(&editor, |this, editor, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.state.draft = editor.read(cx).value().to_string();
                let saved = SavedDraft {
                    content: this.state.draft.clone(),
                    base: this.state.base.clone(),
                };
                this.write_draft(Some(saved), true, cx);
                cx.notify();
            }
        });
        let mut this = Self {
            community,
            channel,
            view: View::Edit,
            state: EditorState {
                base: saved.as_ref().and_then(|s| s.base.clone()),
                draft,
                ..EditorState::default()
            },
            saved,
            editor,
            available: false,
            busy: false,
            error: None,
            confirm_reload: false,
            operation: 0,
            drafts: DraftWriter::default(),
            history: None,
            _subscriptions: vec![subscription],
        };
        let session = this.community.read(cx).session.clone();
        cx.spawn(async move |this, cx| {
            let available = session.canvas_available().await;
            let _ = this.update(cx, |this, cx| {
                this.available = available;
                cx.notify();
            });
        })
        .detach();
        this.load(false, window, cx);
        this
    }

    fn partition(&self, cx: &App) -> Partition {
        self.community.read(cx).partition()
    }

    /// Saves `draft` locally, or removes it; once typing pauses when
    /// `later`.
    fn write_draft(&mut self, draft: Option<SavedDraft>, later: bool, cx: &App) {
        let (partition, key) = (self.partition(cx), draft_key(&self.channel));
        self.drafts.save(partition, key, draft, later, cx);
    }

    fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        cx.notify();
    }

    /// Replaces the draft with `content` started from `base`, and saves it.
    fn adopt(&mut self, revision: Option<&Revision>, window: &mut Window, cx: &mut Context<Self>) {
        let content = revision.map(|r| r.content.clone()).unwrap_or_default();
        self.state.draft = content.clone();
        self.state.base = revision.map(|r| r.id.to_hex());
        self.editor.update(cx, |editor, cx| {
            editor.set_value(content.clone(), window, cx)
        });
        self.write_draft(
            Some(SavedDraft {
                content,
                base: self.state.base.clone(),
            }),
            false,
            cx,
        );
    }

    /// Reads the current Canvas. A first read without a saved draft, or
    /// `replace`, adopts it; otherwise a moved-on Canvas is reported and
    /// the draft kept.
    fn load(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.operation += 1;
        let generation = self.operation;
        self.error = None;
        self.set_busy(true, cx);
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.canvas_read(&channel, true).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.operation != generation {
                    return;
                }
                match result {
                    Ok(head) => {
                        let was_loaded = this.state.loaded;
                        this.state.head = head.clone();
                        this.state.loaded = true;
                        if replace || (this.saved.is_none() && !was_loaded) {
                            this.adopt(head.as_ref(), window, cx);
                        } else if this.state.head_id() != this.state.base {
                            this.error = Some(STALE.into());
                        }
                    }
                    Err(error) => this.error = Some(js_error(&error).into()),
                }
                this.set_busy(false, cx);
            });
        })
        .detach();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.can_save(self.available) || self.busy {
            return;
        }
        self.operation += 1;
        let generation = self.operation;
        self.error = None;
        self.set_busy(true, cx);
        let session = self.community.read(cx).session.clone();
        let (channel, draft) = (self.channel.clone(), self.state.draft.clone());
        let base = self
            .state
            .base
            .as_deref()
            .and_then(|base| EventId::from_hex(base).ok());
        cx.spawn_in(window, async move |this, cx| {
            let result = session.canvas_save(&channel, &draft, base).await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.operation != generation {
                    return;
                }
                match result {
                    Ok(saved) => {
                        this.state.base = Some(saved.id.to_hex());
                        this.state.head = Some(saved);
                        this.write_draft(None, false, cx);
                        this.set_busy(false, cx);
                        cx.emit(Dismissed);
                        return;
                    }
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                this.set_busy(false, cx);
            });
        })
        .detach();
    }

    /// Publishes `revision`'s content as a new revision on top of the
    /// displayed head. A clean editor adopts the result; an edited one
    /// keeps its draft and base. Reports the error, if any, to History.
    fn restore(
        &mut self,
        revision: Revision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        self.operation += 1;
        let generation = self.operation;
        self.set_busy(true, cx);
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.clone();
        let expected = self.state.head.as_ref().map(|head| head.id);
        cx.spawn_in(window, async move |this, cx| {
            let result = session
                .canvas_save(&channel, &revision.content, expected)
                .await;
            // Even an error may follow a delivered write: show the head as
            // it is now, but never adopt or retry on that outcome.
            let head = match &result {
                Ok(_) => None,
                Err(_) => Some(session.canvas_read(&channel, true).await),
            };
            this.update_in(cx, |this, window, cx| {
                let current = this.operation == generation;
                let outcome = match result {
                    Ok(saved) => {
                        if current {
                            let dirty = this.state.dirty();
                            this.state.head = Some(saved.clone());
                            if dirty {
                                this.error = Some(RESTORED_DIRTY.into());
                            } else {
                                this.adopt(Some(&saved), window, cx);
                                this.error = None;
                            }
                        }
                        Ok(())
                    }
                    Err(error) => {
                        if current {
                            if let Some(Ok(head)) = head {
                                this.state.head = head;
                            }
                            this.error = Some(error.to_string().into());
                        }
                        Err(error.to_string())
                    }
                };
                if current {
                    this.set_busy(false, cx);
                }
                outcome
            })
            .unwrap_or_else(|_| Err("The dialog closed".into()))
        })
    }

    /// Closes unless a load, save or restore is running.
    pub fn request_close(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            self.operation += 1;
            cx.emit(Dismissed);
        }
    }

    fn show(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.view == view {
            return;
        }
        self.view = view;
        // History mounts each time it is shown, and reads then.
        self.history = (view == View::History).then(|| {
            let dialog = cx.entity().downgrade();
            cx.new(|cx| {
                History::new(
                    self.community.clone(),
                    self.channel.clone(),
                    dialog,
                    window,
                    cx,
                )
            })
        });
        cx.notify();
    }

    fn reload_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let cancel = || {
            cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.confirm_reload = false;
                cx.notify();
            })
        };
        Dialog::new("canvas-reload", "Reload saved Canvas?")
            .on_close(cancel())
            .child("Discard your draft and reload the saved Canvas?")
            .footer(
                Button::new("canvas-reload-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .on_click(cancel()),
            )
            .footer(
                Button::new("canvas-reload-confirm", "Discard and reload")
                    .destructive()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.confirm_reload = false;
                        this.load(true, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn edit_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = cx.colors();
        div()
            .flex()
            .flex_col()
            .gap(rems(1.))
            .child(div().text_body().text_color(c.text_subtle).child(INTRO))
            .child(
                TextArea::new(&self.editor)
                    .code()
                    .rows(16)
                    .surface(Surface::Floating)
                    .disabled(self.busy || !self.state.loaded),
            )
            .children(
                self.error
                    .clone()
                    .map(|message| error(message, cx).text_body()),
            )
            .into_any_element()
    }
}

impl Render for CanvasDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.view;
        let tab = |id: &'static str, label: &'static str, target: View| {
            Tab::new(id, label)
                .selected(view == target)
                .on_click(cx.listener(move |this, _, window, cx| this.show(target, window, cx)))
        };
        let tabs = Tabs::new(TabVariant::Panel)
            .tab(tab("canvas-tab-edit", "Edit", View::Edit))
            .tab(tab("canvas-tab-history", "History", View::History));
        let body = match (view, &self.history) {
            (View::History, Some(history)) => history.clone().into_any_element(),
            _ => self.edit_view(cx),
        };
        let mut dialog = Dialog::new("channel-canvas", "Channel Canvas")
            .on_close(cx.listener(|this, _, _, cx| this.request_close(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .child(tabs)
                    .child(body),
            );
        if view == View::Edit {
            if self.error.is_some() {
                let loaded = self.state.loaded;
                dialog = dialog.footer(
                    Button::new(
                        "canvas-reload",
                        if loaded {
                            "Reload saved Canvas"
                        } else {
                            "Retry loading"
                        },
                    )
                    .surface(Surface::Floating)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.state.loaded {
                            this.confirm_reload = true;
                            cx.notify();
                        } else {
                            this.load(false, window, cx);
                        }
                    })),
                );
            }
            dialog = dialog.footer(
                Button::new("canvas-save", "Save Canvas")
                    .prominent()
                    .loading(self.busy)
                    .disabled(!self.state.can_save(self.available))
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            );
        }
        let reload = self.confirm_reload.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.confirm_reload = false;
                    cx.notify();
                });
            });
            overlay::modal(window, self.reload_dialog(cx), Some(dismiss))
        });
        div().child(dialog).children(reload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn revision(id: u8, content: &str) -> Revision {
        Revision {
            id: EventId::from_byte_array([id; 32]),
            author: "a".repeat(64),
            created_at: 1,
            content: content.into(),
            kind: 40100,
            tags: Vec::new(),
            sig: String::new(),
        }
    }

    #[test]
    fn drafts_round_trip_in_buzz_app_shape() {
        let draft = SavedDraft {
            content: "# Plan".into(),
            base: None,
        };
        let json = serde_json::to_string(&draft).unwrap();
        assert_eq!(json, r##"{"content":"# Plan","base":null}"##);
        assert_eq!(parse_draft(&json), Some(draft));
        assert_eq!(
            parse_draft(r#"{"content":"x","base":"abc"}"#),
            Some(SavedDraft {
                content: "x".into(),
                base: Some("abc".into())
            })
        );
        // Missing or mistyped fields are ignored, as buzz-app does.
        assert_eq!(parse_draft(r#"{"content":"x"}"#), None);
        assert_eq!(parse_draft(r#"{"content":1,"base":null}"#), None);
        assert_eq!(parse_draft(r#"{"content":"x","base":2}"#), None);
        assert_eq!(parse_draft("null"), None);
        assert_eq!(draft_key("c1"), "canvas-draft-v1:c1");
    }

    #[test]
    fn save_needs_a_loaded_current_base() {
        let head = revision(1, "saved");
        let mut state = EditorState {
            head: Some(head.clone()),
            base: Some(head.id.to_hex()),
            draft: "saved".into(),
            loaded: false,
        };
        assert!(!state.can_save(true));
        state.loaded = true;
        assert!(state.can_save(true));
        assert!(!state.can_save(false));
        assert!(!state.dirty());
        state.draft = "edited".into();
        assert!(state.dirty() && state.can_save(true));
        // A stale base blocks saving even when the text matches.
        state.draft = "saved".into();
        state.base = Some(revision(2, "").id.to_hex());
        assert!(state.dirty() && !state.can_save(true));
        // No Canvas yet: the base is none.
        let empty = EditorState {
            loaded: true,
            ..EditorState::default()
        };
        assert!(empty.can_save(true) && !empty.dirty());
    }

    #[test]
    fn read_errors_read_like_javascript() {
        assert_eq!(
            js_error(&CanvasError::Failed("Relay read timed out".into())),
            "Error: Relay read timed out"
        );
        assert!(
            js_error(&CanvasError::Conflict).starts_with("CanvasConflictError: Canvas changed")
        );
    }
}
