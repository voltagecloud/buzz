//! Inbox → Drafts (`DraftsView.tsx`): the composer drafts saved on this
//! device for the community, each opening its conversation with the
//! draft in the composer.
//!
//! Drafts are the composer's own saved view state (`draft:<channel>`,
//! `draft:<channel>:thread:<root>`); this view only lists them, and
//! deleting one removes that key.

use super::InboxPage;
use super::model::{self, Coordinates};
use crate::community::{AppState, Community};
use crate::ui::channel::{ChannelPage, OpenThread};
use crate::ui::composer::ComposerEvent;
use crate::ui::identity::{self, AgentLibrary};
use crate::ui::navigation::{Place, Request, Target, with_workspace};
use crate::ui::thread::{OnClose, ThreadPanel};
use buzz_relay_client::{ChannelSummary, ChannelType, EventId};
use buzz_ui::{
    ActiveTheme, AlertDialog, Avatar, AvatarSize, Button, ButtonSize, Icon, IconButton,
    IconButtonSize, IconName, PanelHeader, Surface, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;
use std::time::Duration;

/// Drafts listed before "Show more" (and each time it is pressed).
const PAGE: usize = 50;
/// The most drafts listed at all.
const LIMIT: usize = 500;
/// Larger saved drafts are counted, not shown.
const MAX_DRAFT: usize = 8 * 1024 * 1024;
/// How often the list re-reads the saved drafts while it shows.
const POLL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    key: String,
    text: String,
    coordinates: Coordinates,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Saved {
    entries: Vec<Entry>,
    limited: bool,
    unavailable: usize,
}

/// The Drafts view's state, kept by the page while the view shows.
pub(super) struct State {
    saved: Saved,
    selected: Option<String>,
    detail: Option<Entity<DraftDetail>>,
    limit: usize,
    confirm_delete: bool,
    error: Option<SharedString>,
    _poll: Option<Task<()>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            saved: Saved::default(),
            selected: None,
            detail: None,
            limit: PAGE,
            confirm_delete: false,
            error: None,
            _poll: None,
        }
    }
}

/// Text of a saved composer draft; the composer saves `{text, tokens}`.
fn draft_text(value: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    match &value {
        serde_json::Value::String(text) => Some(text.clone()),
        _ => value.get("text")?.as_str().map(str::to_owned),
    }
}

fn list(community: &Entity<Community>, selected: Option<&str>, cx: &App) -> Saved {
    let partition = community.read(cx).partition();
    let rows = cx
        .global::<AppState>()
        .store
        .view_states(&partition, "draft:")
        .unwrap_or_default();
    let mut saved = Saved::default();
    for (key, value) in rows {
        let Some(coordinates) = model::draft_coordinates(&key) else {
            continue;
        };
        if value.len() > MAX_DRAFT {
            saved.unavailable += 1;
            continue;
        }
        let Some(text) = draft_text(&value) else {
            continue;
        };
        // An emptied selected draft stays while its composer is edited.
        if text.trim().is_empty() && selected != Some(key.as_str()) {
            continue;
        }
        if saved.entries.len() == LIMIT {
            saved.limited = true;
            break;
        }
        saved.entries.push(Entry {
            key,
            text,
            coordinates,
        });
    }
    saved
}

impl InboxPage {
    /// Starts listing drafts, and re-reads them while the view shows.
    pub(super) fn open_drafts(&mut self, cx: &mut Context<Self>) {
        self.drafts = State::default();
        self.reload_drafts(cx);
        self.drafts._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |this, cx| this.reload_drafts(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    fn reload_drafts(&mut self, cx: &mut Context<Self>) {
        let Some(community) = &self.community else {
            return;
        };
        let saved = list(community, self.drafts.selected.as_deref(), cx);
        if saved != self.drafts.saved {
            self.drafts.saved = saved;
            cx.notify();
        }
    }

    /// A channel the draft can still be sent in.
    fn draft_channel(&self, channel_id: &str, cx: &App) -> Option<ChannelSummary> {
        let community = self.community.as_ref()?.read(cx);
        let viewer = community.session.viewer().to_string();
        community
            .snapshot
            .channels
            .iter()
            .find(|channel| {
                channel.id == channel_id
                    && !channel.cached
                    && !channel.archived
                    && !channel.read_only
                    && channel.members.contains(&viewer)
            })
            .cloned()
    }

    fn destination(&self, channel: Option<&ChannelSummary>, cx: &App) -> String {
        let Some(channel) = channel else {
            return "Unavailable conversation".into();
        };
        if channel.channel_type != Some(ChannelType::Dm) {
            return format!("#{}", channel.name);
        }
        let Some(community) = &self.community else {
            return "Direct message".into();
        };
        let community = community.read(cx);
        let names = identity::names(
            &community.snapshot.profiles,
            &AgentLibrary::from_hosts(cx),
            community.session.viewer(),
            Some(&community.origin),
            None,
        );
        model::dm_label(
            channel.participants.as_deref(),
            |key, fallback| names.name(key, fallback),
            "Notes to self",
        )
    }

    fn select_draft(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = self.community.clone() else {
            return;
        };
        self.drafts.confirm_delete = false;
        self.drafts.error = None;
        if self.drafts.selected.as_deref() == Some(entry.key.as_str()) {
            return;
        }
        self.drafts.selected = Some(entry.key.clone());
        let channel = self.draft_channel(&entry.coordinates.channel_id, cx);
        let title = match &channel {
            None => "Unavailable conversation".to_string(),
            Some(channel) if channel.channel_type == Some(ChannelType::Dm) => {
                "Direct message".into()
            }
            Some(channel) => channel.name.clone(),
        };
        let dm = channel
            .as_ref()
            .is_some_and(|channel| channel.channel_type == Some(ChannelType::Dm));
        let coordinates = entry.coordinates.clone();
        let detail =
            cx.new(|cx| DraftDetail::new(community, channel, coordinates, title, dm, window, cx));
        cx.subscribe(&detail, |this, _, event: &DraftEvent, cx| match event {
            DraftEvent::Close | DraftEvent::Sent => this.retire_draft(cx),
            DraftEvent::Delete => {
                this.drafts.confirm_delete = true;
                cx.notify();
            }
            DraftEvent::OpenFailed => {
                this.drafts.error = Some("This conversation is no longer available.".into());
                cx.notify();
            }
        })
        .detach();
        self.drafts.detail = Some(detail);
        cx.notify();
    }

    /// Closes the selected draft (Close, Escape, a send or a delete).
    fn retire_draft(&mut self, cx: &mut Context<Self>) {
        self.drafts.selected = None;
        self.drafts.detail = None;
        self.drafts.confirm_delete = false;
        self.drafts.error = None;
        self.reload_drafts(cx);
        cx.notify();
    }

    fn delete_draft(&mut self, cx: &mut Context<Self>) {
        let (Some(key), Some(community)) = (self.drafts.selected.clone(), self.community.clone())
        else {
            return;
        };
        // The open composer saves its text when it goes, so it goes first.
        self.retire_draft(cx);
        let partition = community.read(cx).partition();
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                let removed = cx
                    .global::<AppState>()
                    .store
                    .set_view_state(&partition, &key, None);
                if let Err(error) = removed {
                    this.drafts.error = Some(error.to_string().into());
                }
                this.reload_drafts(cx);
            });
        })
        .detach();
    }

    pub(super) fn drafts_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let ix = c.interaction(Surface::Panel);
        let saved = self.drafts.saved.clone();
        let visible: Vec<Entry> = saved
            .entries
            .iter()
            .take(self.drafts.limit)
            .cloned()
            .collect();
        let mut children: Vec<AnyElement> = Vec::new();
        if saved.entries.is_empty() && saved.unavailable == 0 && self.drafts.selected.is_none() {
            children.push(
                Self::notice(vec![
                    div().text_body().child("No drafts").into_any_element(),
                ])
                .into_any_element(),
            );
        }
        let media = self
            .community
            .as_ref()
            .map(|community| community.read(cx).session.media().clone());
        let profiles = self
            .community
            .as_ref()
            .map(|community| community.read(cx).snapshot.profiles.clone());
        let mut rows = Vec::new();
        for entry in visible {
            let channel = self.draft_channel(&entry.coordinates.channel_id, cx);
            let label = self.destination(channel.as_ref(), cx);
            let dm = channel
                .as_ref()
                .is_some_and(|channel| channel.channel_type == Some(ChannelType::Dm));
            let first = channel
                .as_ref()
                .filter(|_| dm)
                .and_then(|channel| channel.participants.as_ref()?.first().cloned());
            let leading = match &first {
                Some(first) => {
                    let profile = profiles.as_ref().and_then(|profiles| profiles.get(first));
                    let mut avatar = Avatar::new(label.clone()).size(AvatarSize::Default);
                    if profile.is_some_and(|p| p.is_agent) {
                        avatar = avatar.squircle();
                    }
                    if let (Some(picture), Some(media)) =
                        (profile.and_then(|p| p.picture.clone()), &media)
                    {
                        avatar = avatar.image(crate::ui::media::image(&picture, media));
                    }
                    avatar.into_any_element()
                }
                None => Icon::new(IconName::Hash).size(20.).into_any_element(),
            };
            let selected = self.drafts.selected.as_deref() == Some(entry.key.as_str());
            let kind = format!(
                "{} · {}",
                if entry.coordinates.thread_root.is_some() {
                    "Reply"
                } else {
                    "Message"
                },
                if dm { "DM" } else { "Channel" }
            );
            let click = entry.clone();
            rows.push(
                div()
                    .id(SharedString::from(format!("draft-{}", entry.key)))
                    .flex()
                    .items_center()
                    .gap(space::ROW_GAP)
                    .min_h(rems(2.))
                    .px(rems(1.))
                    .py(rems(0.25))
                    .rounded(radius::ROW)
                    .cursor_pointer()
                    .when(selected, |this| this.bg(ix.selected))
                    .when(!selected, |this| this.hover(move |s| s.bg(ix.row_hover)))
                    .child(div().flex_none().child(leading))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(space::S1)
                            .py(space::ROW_GAP)
                            .child(
                                div()
                                    .truncate()
                                    .text_label_sm()
                                    .text_color(c.text_standard)
                                    .child(label),
                            )
                            .child(
                                div().flex().child(
                                    div()
                                        .rounded(radius::CHIP)
                                        .px(space::CHIP_INSET)
                                        .bg(c.affordance_subtle)
                                        .text_color(c.text_subtle)
                                        .text_caption()
                                        .child(kind),
                                ),
                            )
                            .child(
                                div()
                                    .text_body()
                                    .text_color(c.text_subtle)
                                    .line_clamp(2)
                                    .child(entry.text.clone()),
                            ),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.select_draft(&click, window, cx)
                        }),
                    )
                    .into_any_element(),
            );
        }
        children.push(
            div()
                .flex()
                .flex_col()
                .px(space::ROW_GAP)
                .pb(space::PANEL_INSET)
                .children(rows)
                .into_any_element(),
        );
        if saved.unavailable > 0 {
            children.push(
                Self::notice(vec![
                    div()
                        .text_body()
                        .child(format!(
                            "{} saved drafts exceed the preview size limit. They remain in their \
                     conversations.",
                            saved.unavailable
                        ))
                        .into_any_element(),
                ])
                .into_any_element(),
            );
        }
        if saved.limited {
            children.push(
                Self::notice(vec![
                    div()
                        .text_body()
                        .child(
                            "Showing the first 500 saved drafts. Other drafts remain in their \
                     conversations.",
                        )
                        .into_any_element(),
                ])
                .into_any_element(),
            );
        }
        if saved.entries.len() > self.drafts.limit {
            children.push(
                Self::notice(vec![
                    Button::new("drafts-more", "Show more")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.drafts.limit += PAGE;
                            cx.notify();
                        }))
                        .into_any_element(),
                ])
                .into_any_element(),
            );
        }
        div()
            .id("drafts-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .children(children)
            .into_any_element()
    }

    pub(super) fn draft_detail(&self) -> Option<AnyElement> {
        self.drafts
            .detail
            .clone()
            .map(IntoElement::into_any_element)
    }

    pub(super) fn draft_error(&self) -> Option<SharedString> {
        self.drafts.error.clone()
    }

    /// "Delete draft?" over the page.
    pub(super) fn draft_delete_dialog(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.drafts.confirm_delete {
            return None;
        }
        let page = cx.entity().downgrade();
        let dismiss: crate::ui::overlay::Dismiss = std::rc::Rc::new(move |_, cx| {
            let _ = page.update(cx, |this, cx| {
                this.drafts.confirm_delete = false;
                cx.notify();
            });
        });
        let dialog = AlertDialog::new("Delete draft?")
            .description(
                "Delete this saved draft and its attachments on this device? This cannot be \
                 undone.",
            )
            .action(
                Button::new("draft-delete-confirm", "Delete draft")
                    .size(ButtonSize::Sm)
                    .destructive()
                    .on_click(cx.listener(|this, _, _, cx| this.delete_draft(cx))),
            )
            .action(
                Button::new("draft-delete-cancel", "Cancel")
                    .size(ButtonSize::Sm)
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.drafts.confirm_delete = false;
                        cx.notify();
                    })),
            );
        Some(crate::ui::overlay::modal(window, dialog, Some(dismiss)).into_any_element())
    }
}

enum DraftEvent {
    Close,
    Delete,
    Sent,
    OpenFailed,
}

/// The selected draft: its conversation with the draft in the composer.
pub(super) struct DraftDetail {
    community: Entity<Community>,
    channel: Option<ChannelSummary>,
    coordinates: Coordinates,
    title: String,
    dm: bool,
    reader: Option<AnyView>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DraftEvent> for DraftDetail {}

impl DraftDetail {
    fn new(
        community: Entity<Community>,
        channel: Option<ChannelSummary>,
        coordinates: Coordinates,
        title: String,
        dm: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut subscriptions = Vec::new();
        let reader: Option<AnyView> = channel.clone().map(|channel| {
            let root = coordinates
                .thread_root
                .as_deref()
                .and_then(|root| EventId::from_hex(root).ok());
            match root {
                Some(root) => {
                    let detail = cx.entity().downgrade();
                    let on_close: OnClose = Arc::new(move |_, cx| {
                        let _ = detail.update(cx, |_, cx| cx.emit(DraftEvent::Close));
                    });
                    let panel = cx.new(|cx| {
                        let mut panel = ThreadPanel::new(
                            community.clone(),
                            channel.id.clone(),
                            root,
                            on_close,
                            window,
                            cx,
                        );
                        panel.require_ready_root = true;
                        // A reader's links open nowhere in Buzz
                        // (`onOpenLink={() => false}`).
                        panel.as_reader(cx);
                        panel
                    });
                    let composer = panel.read(cx).composer_entity();
                    subscriptions.push(cx.subscribe(
                        &composer,
                        |_, _, event: &ComposerEvent, cx| {
                            if matches!(event, ComposerEvent::Submit(_)) {
                                cx.emit(DraftEvent::Sent);
                            }
                        },
                    ));
                    panel.into()
                }
                None => {
                    let origin = community.read(cx).origin.to_string();
                    let on_open_thread: OpenThread = Arc::new(move |row, _, _, cx| {
                        let root = row.thread_root_id.unwrap_or(row.id);
                        let target = Target {
                            community: Some(origin.clone()),
                            place: Place::Conversation {
                                channel: row.channel.to_string(),
                                message: Some(root),
                                thread: Some(root),
                                members: false,
                            },
                        };
                        with_workspace(cx, move |workspace, window, cx| {
                            workspace.handle(Request::Open(target), window, cx)
                        });
                    });
                    let page = cx.new(|cx| {
                        let mut page = ChannelPage::new(
                            community.clone(),
                            channel,
                            on_open_thread,
                            window,
                            cx,
                        );
                        page.secondary = true;
                        page.as_reader(cx);
                        page
                    });
                    let composer = page.read(cx).composer_entity();
                    subscriptions.push(cx.subscribe(
                        &composer,
                        |_, _, event: &ComposerEvent, cx| {
                            if matches!(event, ComposerEvent::Submit(_)) {
                                cx.emit(DraftEvent::Sent);
                            }
                        },
                    ));
                    page.into()
                }
            }
        });
        Self {
            community,
            channel,
            coordinates,
            title,
            dm,
            reader,
            focus,
            _subscriptions: subscriptions,
        }
    }

    fn open(&self, cx: &mut Context<Self>) {
        let Some(channel) = &self.channel else {
            cx.emit(DraftEvent::OpenFailed);
            return;
        };
        let root = self
            .coordinates
            .thread_root
            .as_deref()
            .and_then(|root| EventId::from_hex(root).ok());
        let target = Target {
            community: Some(self.community.read(cx).origin.to_string()),
            place: Place::Conversation {
                channel: channel.id.clone(),
                message: root,
                thread: root,
                members: false,
            },
        };
        with_workspace(cx, move |workspace, window, cx| {
            workspace.handle(Request::Open(target), window, cx)
        });
    }
}

impl Render for DraftDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let mut header = PanelHeader::new()
            .joined(true)
            .label(
                Some(if self.dm {
                    IconName::ChatCircle
                } else {
                    IconName::Hash
                }),
                self.title.clone(),
            )
            .action(
                IconButton::new("draft-delete", IconName::Trash)
                    .size(IconButtonSize::Sm)
                    .tooltip("Delete draft")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DraftEvent::Delete))),
            );
        if self.channel.is_some() {
            header = header.action(
                IconButton::new("draft-open", IconName::ArrowSquareOut)
                    .size(IconButtonSize::Sm)
                    .tooltip("Open in origin")
                    .on_click(cx.listener(|this, _, _, cx| this.open(cx))),
            );
        }
        header = header.action(
            IconButton::new("draft-close", IconName::X)
                .size(IconButtonSize::Sm)
                .tooltip("Close detail")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(DraftEvent::Close))),
        );
        div()
            .id("draft-detail")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    cx.emit(DraftEvent::Close);
                }
            }))
            .child(header)
            .child(div().flex().flex_col().flex_1().min_h_0().map(|this| {
                match &self.reader {
                    Some(reader) => this.child(reader.clone()),
                    None => this.child(
                        div()
                            .py(space::S3)
                            .px(space::PANEL_INSET)
                            .text_body()
                            .text_color(c.text_subtle)
                            .child("This conversation is unavailable. Your draft is unchanged."),
                    ),
                }
            }))
    }
}
