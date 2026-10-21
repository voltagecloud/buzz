//! The Canvas dialog's History tab (`CanvasHistory.tsx`): saved revisions
//! in pages of 25, a read-only preview of the selected one, and "Restore
//! this version" behind a confirmation. Restoring goes through the
//! dialog's save, with the displayed head as its precondition.

use super::{CanvasDialog, js_error};
use crate::community::Community;
use crate::ui::message::display_name;
use crate::ui::modal::error;
use crate::ui::overlay::{self, Dismiss};
use buzz_relay_client::PublicKey;
use buzz_relay_client::canvas::{HistoryCursor, HistoryPage, Revision};
use buzz_ui::{ActiveTheme, Button, Dialog, Radio, Surface, TextArea, TypeStyled};
use gpui_kit::component::input::TextareaState;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nostr::nips::nip19::ToBech32;
use std::collections::HashMap;
use std::rc::Rc;

const RESTORED: &str = "Version restored as a new revision.";
const TOO_LARGE: &str = "This version exceeds the 24 KiB Canvas limit and cannot be restored here.";
const CONFIRM: &str =
    "This publishes the selected version as a new Canvas revision. Existing history is kept.";
const CONFIRM_DIRTY: &str = "Your unsaved draft will be kept separately. Copy any edits you want to keep before reloading the restored Canvas.";

/// `publicKeyLabels`: `npub…` and the shortest suffix (3 or more) that
/// tells these keys apart.
fn key_labels<'a>(keys: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let encoded: Vec<(String, String)> = keys
        .into_iter()
        .filter_map(|key| {
            let npub = PublicKey::from_hex(key).ok()?.to_bech32().ok()?;
            Some((key.to_ascii_lowercase(), npub))
        })
        .collect::<HashMap<_, _>>()
        .into_iter()
        .collect();
    let tail = |npub: &str, length: usize| npub[npub.len() - length.min(npub.len())..].to_owned();
    let mut length = 3;
    while length < 58 {
        let unique: std::collections::HashSet<String> =
            encoded.iter().map(|(_, npub)| tail(npub, length)).collect();
        if unique.len() >= encoded.len() {
            break;
        }
        length += 1;
    }
    encoded
        .into_iter()
        .map(|(key, npub)| (key, format!("npub…{}", tail(&npub, length))))
        .collect()
}

/// `toLocaleString()` in en-US: "10/9/2026, 3:05:09 PM".
fn locale_time(seconds: u64) -> String {
    chrono::DateTime::from_timestamp(seconds as i64, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%-m/%-d/%Y, %-I:%M:%S %p")
                .to_string()
        })
        .unwrap_or_default()
}

/// Appends an older page, or replaces the list with a first page.
fn merge(previous: Option<HistoryPage>, page: HistoryPage, older: bool) -> HistoryPage {
    if !older {
        return page;
    }
    let mut revisions = previous.map(|p| p.revisions).unwrap_or_default();
    revisions.extend(page.revisions);
    HistoryPage {
        revisions,
        next: page.next,
    }
}

/// The selection after a first page: the same revision if listed,
/// otherwise the newest.
fn reselect(previous: Option<&Revision>, page: &HistoryPage) -> Option<Revision> {
    previous
        .and_then(|selected| page.revisions.iter().find(|r| r.id == selected.id))
        .or_else(|| page.revisions.first())
        .cloned()
}

fn loaded_notice(count: usize) -> String {
    if count > 0 {
        format!("{count} revisions loaded.")
    } else {
        "No more revisions.".into()
    }
}

pub(super) struct History {
    community: Entity<Community>,
    channel: String,
    dialog: WeakEntity<CanvasDialog>,
    page: Option<HistoryPage>,
    selected: Option<Revision>,
    preview: Entity<TextareaState>,
    loading: bool,
    error: String,
    notice: String,
    confirm: bool,
    generation: u64,
    _observe: Option<Subscription>,
}

impl History {
    pub(super) fn new(
        community: Entity<Community>,
        channel: String,
        dialog: WeakEntity<CanvasDialog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preview = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx);
            state.set_readonly(true, cx);
            state
        });
        // Head, busy and draft state come from the dialog.
        let _observe = dialog
            .upgrade()
            .map(|dialog| cx.observe(&dialog, |_, _, cx| cx.notify()));
        let mut this = Self {
            community,
            channel,
            dialog,
            page: None,
            selected: None,
            preview,
            loading: false,
            error: String::new(),
            notice: String::new(),
            confirm: false,
            generation: 0,
            _observe,
        };
        this.load(None, window, cx);
        this
    }

    fn select(&mut self, revision: Option<Revision>, window: &mut Window, cx: &mut Context<Self>) {
        let content = revision
            .as_ref()
            .map(|r| r.content.clone())
            .unwrap_or_default();
        if self.selected.as_ref().map(|r| r.id) != revision.as_ref().map(|r| r.id) {
            self.preview
                .update(cx, |preview, cx| preview.set_value(content, window, cx));
        }
        self.selected = revision;
        cx.notify();
    }

    /// Profiles of the listed authors, for their names.
    fn want_profiles(&self, cx: &App) {
        let Some(page) = &self.page else { return };
        let mut keys: Vec<String> = page.revisions.iter().map(|r| r.author.clone()).collect();
        keys.sort();
        keys.dedup();
        self.community.read(cx).session.want_profiles(keys);
    }

    fn load(&mut self, cursor: Option<HistoryCursor>, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.error.clear();
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.canvas_history(&channel, cursor).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        let count = page.revisions.len();
                        let first = page.clone();
                        this.page = Some(merge(this.page.take(), page, cursor.is_some()));
                        if cursor.is_none() {
                            let selected = reselect(this.selected.as_ref(), &first);
                            this.select(selected, window, cx);
                        }
                        this.notice = loaded_notice(count);
                        this.want_profiles(cx);
                    }
                    Err(error) => this.error = js_error(&error),
                }
                this.loading = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Recheck the head without replacing the editor's draft.
        let _ = self
            .dialog
            .update(cx, |dialog, cx| dialog.load(false, window, cx));
        self.load(None, window, cx);
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let Ok(restore) = self
            .dialog
            .update(cx, |dialog, cx| dialog.restore(selected, window, cx))
        else {
            return;
        };
        cx.notify();
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.clone();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = restore.await;
            let current = this
                .update(cx, |this, cx| {
                    if this.generation != generation {
                        return false;
                    }
                    match outcome {
                        Ok(()) => this.notice = RESTORED.into(),
                        Err(message) => this.error = message,
                    }
                    this.confirm = false;
                    this.loading = true;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            // Even an error can follow an accepted publication: reconcile
            // without clearing its outcome or retrying the write.
            let result = session.canvas_history(&channel, None).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        let selected = reselect(this.selected.as_ref(), &page);
                        this.page = Some(page);
                        this.select(selected, window, cx);
                        this.want_profiles(cx);
                    }
                    Err(error) => {
                        let previous = std::mem::take(&mut this.error);
                        let lead = if previous.is_empty() {
                            String::new()
                        } else {
                            format!("{previous} ")
                        };
                        this.error = format!("{lead}History refresh failed: {}", js_error(&error));
                    }
                }
                this.loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_dialog(&self, busy: bool, dirty: bool, cx: &mut Context<Self>) -> AnyElement {
        let cancel = || {
            cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.confirm = false;
                cx.notify();
            })
        };
        Dialog::new("canvas-restore", "Restore this version?")
            .when(!busy, |dialog| dialog.on_close(cancel()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .child(CONFIRM)
                    .when(dirty, |this| this.child(CONFIRM_DIRTY)),
            )
            .footer(
                Button::new("canvas-restore-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .disabled(busy)
                    .on_click(cancel()),
            )
            .footer(
                Button::new("canvas-restore-confirm", "Restore version")
                    .prominent()
                    .loading(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
            )
            .into_any_element()
    }
}

impl Render for History {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let (head, busy, loaded, dirty, available) = self
            .dialog
            .upgrade()
            .map(|dialog| {
                let dialog = dialog.read(cx);
                (
                    dialog.state.head.as_ref().map(|head| head.id),
                    dialog.busy,
                    dialog.state.loaded,
                    dialog.state.dirty(),
                    dialog.available,
                )
            })
            .unwrap_or((None, false, false, false, false));
        let profiles = self.community.read(cx).snapshot.profiles.clone();
        let labels = key_labels(
            self.page
                .iter()
                .flat_map(|page| page.revisions.iter().map(|r| r.author.as_str())),
        );
        let name = |key: &str| -> SharedString {
            if profiles.get(key).is_some() {
                display_name(&profiles, key)
            } else {
                labels.get(key).cloned().unwrap_or_default().into()
            }
        };
        let status = if self.loading {
            "Loading history…".to_owned()
        } else {
            self.notice.clone()
        };
        let actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(rems(0.5))
            .child(
                Button::new("canvas-history-refresh", "Refresh history")
                    .surface(Surface::Floating)
                    .loading(self.loading || busy)
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
            )
            .child(div().text_body().child(status));
        let list = self.page.as_ref().map(|page| {
            if page.revisions.is_empty() {
                return div().child("No saved revisions.").into_any_element();
            }
            let selected = self.selected.as_ref().map(|r| r.id);
            let rows = page.revisions.iter().map(|revision| {
                let current = Some(revision.id) == head;
                let label = format!(
                    "{}{}",
                    locale_time(revision.created_at),
                    if current { " · Current" } else { "" }
                );
                let description = format!(
                    "{} · {}",
                    name(&revision.author),
                    &revision.id.to_hex()[..8]
                );
                let (pick, weak) = (revision.clone(), cx.entity().downgrade());
                Radio::new(
                    SharedString::from(format!("revision-{}", revision.id.to_hex())),
                    label,
                    Some(revision.id) == selected,
                )
                .description(description)
                .disabled(busy)
                .on_select(move |window, cx| {
                    let _ = weak.update(cx, |this, cx| this.select(Some(pick.clone()), window, cx));
                })
            });
            div()
                .flex()
                .flex_col()
                .gap(rems(1.))
                .child(
                    div()
                        .id("canvas-revisions")
                        .flex()
                        .flex_col()
                        .gap(rems(0.5))
                        .max_h(rems(12.))
                        .overflow_y_scroll()
                        .child(div().text_label_sm().child("Saved revisions"))
                        .child(div().flex().flex_col().gap(rems(0.75)).children(rows)),
                )
                .when(page.next.is_some(), |this| {
                    let next = page.next;
                    this.child(
                        div().child(
                            Button::new("canvas-history-older", "Load older")
                                .surface(Surface::Floating)
                                .disabled(busy)
                                .loading(self.loading)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.load(next, window, cx)
                                })),
                        ),
                    )
                })
                .into_any_element()
        });
        let preview = self.selected.as_ref().map(|selected| {
            let too_large = selected.too_large();
            let disabled = !loaded
                || busy
                || self.loading
                || !available
                || Some(selected.id) == head
                || too_large;
            div()
                .flex()
                .flex_col()
                .gap(rems(1.))
                .child(
                    TextArea::new(&self.preview)
                        .code()
                        .rows(10)
                        .surface(Surface::Floating),
                )
                .when(selected.content.is_empty(), |this| {
                    this.child(div().child("This version is empty."))
                })
                .when(too_large, |this| this.child(div().child(TOO_LARGE)))
                .child(
                    div().flex().child(
                        Button::new("canvas-restore", "Restore this version")
                            .prominent()
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.error.clear();
                                this.confirm = true;
                                cx.notify();
                            })),
                    ),
                )
        });
        let confirm = self.confirm.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if !busy {
                        this.confirm = false;
                        cx.notify();
                    }
                });
            });
            overlay::modal(window, self.confirm_dialog(busy, dirty, cx), Some(dismiss))
        });
        div()
            .flex()
            .flex_col()
            .gap(rems(1.))
            .text_body()
            .text_color(c.text_standard)
            .child(actions)
            .children(list)
            .when(!self.error.is_empty(), |this| {
                this.child(error(self.error.clone(), cx).text_body())
            })
            .children(preview)
            .children(confirm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::EventId;
    use core::prelude::v1::test;

    fn revision(id: u8) -> Revision {
        Revision {
            id: EventId::from_byte_array([id; 32]),
            author: "a".repeat(64),
            created_at: u64::from(id),
            content: String::new(),
            kind: 40100,
            tags: Vec::new(),
            sig: String::new(),
        }
    }

    #[test]
    fn key_labels_grow_until_unique() {
        let key = |i: u8| {
            let secret = nostr::prelude::SecretKey::from_slice(&[i; 32]).unwrap();
            nostr::prelude::Keys::new(secret).public_key().to_hex()
        };
        let (a, b) = (key(1), key(2));
        let labels = key_labels([a.as_str(), b.as_str()]);
        assert_ne!(labels[&a], labels[&b]);
        assert!(labels[&a].starts_with("npub…"));
        assert!(labels[&a].chars().count() >= "npub…".chars().count() + 3);
        let single = key_labels([a.as_str()]);
        assert_eq!(single[&a].chars().count(), "npub…".chars().count() + 3);
        assert!(key_labels(["not a key"]).is_empty());
    }

    #[test]
    fn pages_append_and_selection_survives() {
        let first = HistoryPage {
            revisions: vec![revision(3), revision(2)],
            next: Some(HistoryCursor {
                until: 2,
                before_id: revision(2).id,
            }),
        };
        let older = HistoryPage {
            revisions: vec![revision(1)],
            next: None,
        };
        let merged = merge(Some(first.clone()), older.clone(), true);
        assert_eq!(merged.revisions.len(), 3);
        assert_eq!(merged.next, None);
        assert_eq!(merge(Some(first.clone()), older, false).revisions.len(), 1);
        assert_eq!(reselect(Some(&revision(2)), &first), Some(revision(2)));
        assert_eq!(reselect(Some(&revision(9)), &first), Some(revision(3)));
        assert_eq!(reselect(None, &HistoryPage::default()), None);
        assert_eq!(loaded_notice(2), "2 revisions loaded.");
        assert_eq!(loaded_notice(0), "No more revisions.");
    }

    #[test]
    fn times_read_like_the_browser_locale() {
        let text = locale_time(1_760_000_000);
        assert!(
            text.contains(", ") && (text.ends_with("AM") || text.ends_with("PM")),
            "{text}"
        );
        assert_eq!(text.matches('/').count(), 2);
    }
}
