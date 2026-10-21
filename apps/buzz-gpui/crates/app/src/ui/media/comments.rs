//! The media viewer's comments sidebar (`MediaReviewViewer.tsx`): the
//! message's thread, every page loaded, as full message rows (reactions,
//! the hover action bar, retry), with a composer that replies to the
//! thread. Comments on a video can carry the current time
//! (`⏱ m:ss — text`, `media-timecode.ts`); their time chips seek it.

use super::super::composer::{Composer, ComposerEvent, Submission};
use super::super::markdown::Directory;
use super::super::message::{self, Layout, Rows};
use super::super::message_actions::{Hooks, MessageActions};
use super::RowMedia;
use super::attachments::OnOpen;
use super::video::VideoPlayer;
use crate::community::Community;
use crate::mirror::mirror;
use buzz_relay_client::fold::{Attachment, MessageRow};
use buzz_relay_client::session::ThreadView;
use buzz_relay_client::{EventId, ThreadSnapshot, ThreadStatus};
use buzz_ui::{ActiveTheme, Checkbox, IconName, PanelHeader, TypeStyled};
use gpui_kit::component::input::TextareaState;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;
use std::sync::Arc;

/// What the thread says about the reviewed attachment.
#[derive(Clone, Debug, PartialEq)]
pub enum ThreadState {
    Loading,
    /// Reading the thread failed; the media still shows.
    Failed(String),
    /// "Original message unavailable."
    Missing,
    Ready,
}

pub struct Comments {
    community: Entity<Community>,
    channel: String,
    root: EventId,
    view: ThreadView,
    snapshot: Arc<ThreadSnapshot>,
    composer: Entity<Composer>,
    input: Entity<TextareaState>,
    /// The viewer's video, when it shows one: comments are stamped with its
    /// time, chips seek it, and typing pauses it.
    video: Option<WeakEntity<VideoPlayer>>,
    at_frame: bool,
    error: Option<SharedString>,
    list: ScrollHandle,
    /// Outlined after its marker was clicked.
    selected: Option<EventId>,
    /// Switches the viewer to an attachment of a comment.
    on_open: Option<OnOpen>,
    /// Row actions: the hover bar, menus, reactions, edits and dialogs.
    actions: Entity<MessageActions>,
    _watch: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Comments {
    pub fn new(
        community: Entity<Community>,
        channel: String,
        root: EventId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = community.read(cx).session.open_thread(&channel, root, true);
        let snapshot = view.snapshot.borrow().clone();
        let watch = mirror(view.snapshot.clone(), cx, |this: &mut Self, value| {
            this.snapshot = value;
            // Every page loads (`useMediaComments`).
            if this.snapshot.can_load_more && this.snapshot.status == ThreadStatus::Ready {
                this.view.load_more();
            }
        });
        let composer = cx.new(|cx| {
            Composer::new(
                community.clone(),
                channel.clone(),
                Some(root),
                "Reply to thread".into(),
                window,
                cx,
            )
        });
        let input = composer.read(cx).input();
        let input_focus = input.read(cx).focus_handle(cx);
        let actions = Self::message_actions(&community, &channel, root, &composer, window, cx);
        let subscriptions = vec![
            cx.observe(&community, |_, _, cx| cx.notify()),
            cx.observe(&actions, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event: &ComposerEvent, window, cx| match event {
                    ComposerEvent::Submit(submission) => this.send(submission.clone(), window, cx),
                    ComposerEvent::EditLast => this.edit_last(window, cx),
                    ComposerEvent::RetryEdit => this
                        .actions
                        .update(cx, |actions, cx| actions.retry_edit(cx)),
                    ComposerEvent::CancelReply => {}
                },
            ),
            // Typing a comment pauses the video (`onFocusCapture`).
            cx.on_focus_in(&input_focus, window, |this, _, cx| {
                if let Some(video) = this.video.as_ref().and_then(WeakEntity::upgrade) {
                    video.update(cx, |video, cx| video.pause(cx));
                }
            }),
        ];
        Self {
            community,
            channel,
            root,
            view,
            snapshot,
            composer,
            input,
            video: None,
            at_frame: true,
            error: None,
            list: ScrollHandle::new(),
            selected: None,
            on_open: None,
            actions,
            _watch: watch,
            _subscriptions: subscriptions,
        }
    }

    /// The row actions, wired to the comments' composer (as the thread
    /// panel wires its own).
    fn message_actions(
        community: &Entity<Community>,
        channel: &str,
        root: EventId,
        composer: &Entity<Composer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<MessageActions> {
        let comments = cx.entity().downgrade();
        let (root_of, find) = (comments.clone(), comments);
        let (reply, edit, focus) = (
            composer.downgrade(),
            composer.downgrade(),
            composer.downgrade(),
        );
        let hooks = Hooks {
            reply: Rc::new(move |_, window, cx| {
                let _ = reply.update(cx, |composer, cx| composer.focus(window, cx));
            }),
            open_thread: None,
            edit: Rc::new(move |row, window, cx| {
                let _ = edit.update(cx, |composer, cx| composer.start_edit(row, window, cx));
            }),
            focus_composer: Rc::new(move |window, cx| {
                let _ = focus.update(cx, |composer, cx| composer.focus(window, cx));
            }),
            thread_root: Some(Rc::new(move |cx| {
                root_of
                    .upgrade()
                    .and_then(|comments| comments.read(cx).snapshot.root.clone())
            })),
            find: Rc::new(move |id, cx| {
                find.upgrade()
                    .and_then(|comments| comments.read(cx).row(id))
            }),
            thread: Some(root),
            composer: composer.downgrade(),
        };
        let (community, channel) = (community.clone(), channel.to_string());
        cx.new(|cx| MessageActions::new(community, channel, hooks, window, cx))
    }

    /// Up in an empty composer edits the viewer's newest editable comment.
    fn edit_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows();
        let row = self.actions.read(cx).last_editable(rows.iter(), cx);
        if let Some(row) = row {
            self.composer
                .update(cx, |composer, cx| composer.start_edit(row, window, cx));
        }
    }

    pub fn set_video(&mut self, video: Option<WeakEntity<VideoPlayer>>, cx: &mut Context<Self>) {
        self.video = video;
        cx.notify();
    }

    pub fn set_on_open(&mut self, on_open: OnOpen) {
        self.on_open = Some(on_open);
    }

    /// Mounts the composer's tools once the thread is ready, open or
    /// closed, as buzz-app keeps the comments mounted; plugins hear the
    /// thread's root (`threadRootId={snapshot.root.id}`).
    pub fn mount_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.snapshot.root.as_ref().map(|root| root.id) else {
            return;
        };
        let ready = self.state() == ThreadState::Ready;
        self.composer.update(cx, |composer, cx| {
            composer.set_thread_root(root, window, cx);
            if ready {
                composer.mount_tools(window, cx);
            }
        });
    }

    pub fn state(&self) -> ThreadState {
        let snapshot = &self.snapshot;
        if let Some(error) = &snapshot.error {
            ThreadState::Failed(error.clone())
        } else if matches!(snapshot.status, ThreadStatus::Loading | ThreadStatus::Idle) {
            ThreadState::Loading
        } else if snapshot.root.is_none() {
            ThreadState::Missing
        } else {
            ThreadState::Ready
        }
    }

    pub fn retry(&self) {
        self.view.refresh();
    }

    /// The root, then every other row once, oldest first.
    pub fn rows(&self) -> Vec<Arc<MessageRow>> {
        let snapshot = &self.snapshot;
        let Some(root) = &snapshot.root else {
            return Vec::new();
        };
        let mut rows = vec![root.clone()];
        rows.extend(self.replies());
        rows
    }

    /// Replies (and the opened message when it is one), oldest first.
    pub fn replies(&self) -> Vec<Arc<MessageRow>> {
        let snapshot = &self.snapshot;
        let root = snapshot.root.as_ref().map(|root| root.id);
        let mut replies: Vec<Arc<MessageRow>> = Vec::new();
        for row in snapshot.target.iter().chain(snapshot.replies.iter()) {
            if Some(row.id) != root && !replies.iter().any(|r| r.id == row.id) {
                replies.push(row.clone());
            }
        }
        replies.sort_by_key(|row| (row.created_at_ms, row.id.to_hex()));
        replies
    }

    pub fn root_id(&self) -> Option<EventId> {
        self.snapshot.root.as_ref().map(|root| root.id)
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Open the sidebar at first: there are comments, or reading failed.
    pub fn has_comments(&self) -> bool {
        !self.replies().is_empty() || self.snapshot.error.is_some()
    }

    /// Outlines a comment and scrolls it into view.
    pub fn select(&mut self, id: EventId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if let Some(index) = self.replies().iter().position(|row| row.id == id) {
            self.list.scroll_to_item(index);
        }
        cx.notify();
    }

    /// Typing in the composer must not drive the viewer's keys.
    pub fn typing(&self, window: &Window, cx: &App) -> bool {
        self.input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    }

    fn row(&self, id: EventId) -> Option<Arc<MessageRow>> {
        self.rows().into_iter().find(|row| row.id == id)
    }

    fn send(&mut self, mut submission: Submission, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = &submission.editing {
            let current = self.row(target.id);
            self.actions.update(cx, |actions, cx| {
                actions.save_edit(submission, current, window, cx)
            });
            return;
        }
        if self.at_frame
            && let Some(video) = self.video.as_ref().and_then(WeakEntity::upgrade)
        {
            let time = video.read(cx).position();
            submission.content = super::timecode::time_reply(time, &submission.content);
        }
        let session = self.community.read(cx).session.clone();
        // The message may itself be a reply: answer its thread's root.
        let root = self.root_id().unwrap_or(self.root);
        let channel = self.channel.clone();
        cx.spawn_in(window, async move |this, cx| {
            // Files upload in the background first; a failed upload is
            // handed back to the composer's draft.
            let Some(attachments) = submission.attachments().await else {
                return;
            };
            let result = session
                .send(
                    &channel,
                    &submission.content,
                    submission.mentions.clone(),
                    attachments,
                    Some((root, root)),
                )
                .await;
            let handed_back = cx
                .update(|_, cx| submission.settle(&result, cx))
                .unwrap_or(false);
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.error = None;
                        this.composer
                            .update(cx, |composer, cx| composer.sent(window, cx));
                        this.list.scroll_to_bottom();
                    }
                    Err(_) if handed_back => {}
                    Err(error) => {
                        this.error = Some(error.into());
                        this.composer.update(cx, |composer, cx| composer.failed(cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The delete and report dialogs, drawn over the viewer.
    pub fn overlays(&self, window: &mut Window, cx: &mut App) -> Vec<AnyElement> {
        self.actions
            .update(cx, |actions, cx| actions.overlays(window, cx))
    }
}

/// Whether `rows` still carry `url` (`attachmentAvailable`).
pub fn carries(rows: &[Arc<MessageRow>], url: &str) -> bool {
    rows.iter()
        .any(|row| row.attachments.iter().any(|a| a.url == url))
}

/// Every image across `rows`, then `current`, each once (`imageAttachments`).
pub fn gallery(rows: &[Arc<MessageRow>], current: &Attachment) -> Vec<Attachment> {
    let mut seen: Vec<Attachment> = Vec::new();
    for item in rows
        .iter()
        .flat_map(|row| row.attachments.iter())
        .chain(std::iter::once(current))
    {
        if item.kind == buzz_relay_client::fold::AttachmentKind::Image
            && !seen.iter().any(|a| a.url == item.url)
        {
            seen.push(item.clone());
        }
    }
    seen
}

impl Render for Comments {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        self.mount_composer(window, cx);
        let state = self.state();
        let resolved = state == ThreadState::Ready;
        let replies = self.replies();
        let count = replies.len();
        let community = self.community.read(cx);
        let members = community
            .snapshot
            .channels
            .iter()
            .find(|channel| channel.id == self.channel)
            .map(|channel| channel.members.clone());
        let mut rows = Rows::new(
            community.session.viewer(),
            Directory::of(community, members.as_deref(), cx),
        );
        rows.in_thread = true;
        MessageActions::fill(&self.actions, &mut rows, cx);
        let video_handle = self.video.clone();
        rows.media = RowMedia {
            // Video timecodes seek the viewer's player and play.
            on_time: video_handle.map(|video| {
                Rc::new(move |seconds: f64, _: &mut Window, cx: &mut App| {
                    let _ = video.update(cx, |video, cx| video.play_from(seconds, cx));
                }) as super::attachments::OnTime
            }),
            on_open: self.on_open.clone(),
            seek: None,
            inline_time: true,
        };
        let rows = Rc::new(rows);
        let video = self.video.as_ref().and_then(WeakEntity::upgrade);
        let header = PanelHeader::new()
            .label(Some(IconName::ChatCircle), "Comments")
            .when(resolved, |header| {
                header.action(
                    div()
                        .px(px(8.))
                        .text_caption()
                        .font_features(super::tabular())
                        .text_color(c.text_subtle)
                        .child(count.to_string()),
                )
            });
        let note = |text: &'static str| {
            div()
                .px(px(12.))
                .py(px(24.))
                .text_center()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
        };
        let body = if resolved {
            let items = message::items(&replies);
            let selected = self.selected;
            let list = div()
                .id("media-comments-list")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.list)
                .px(px(16.))
                .children(items.iter().map(|item| {
                    let outlined = selected == Some(item.row.id);
                    div()
                        .py(px(8.))
                        .mb(px(8.))
                        .border_1()
                        .border_color(if outlined {
                            c.border_prominent
                        } else {
                            transparent_black()
                        })
                        .child(
                            // Its links open nowhere in Buzz
                            // (`onOpenLink={() => false}`).
                            crate::ui::links::with_mode(crate::ui::links::LinkMode::Closed, || {
                                message::render_in(Layout::Nested, item, &rows, cx)
                            }),
                        )
                }))
                .when(count == 0, |d| {
                    d.child(note("No comments yet. Add the first below."))
                })
                .when(self.snapshot.limited, |d| {
                    d.child(note("Thread history limit reached."))
                });
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(list)
                .children(self.error.clone().map(|error| {
                    div()
                        .px(px(16.))
                        .pb(px(4.))
                        .text_caption()
                        .text_color(c.text_danger)
                        .child(error)
                }))
                .when_some(video, |d, video| {
                    let time = video.read(cx).position();
                    let weak = cx.entity().downgrade();
                    // `.mediaReviewTimeOption`: the padded time, then the box.
                    d.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.))
                            .px(px(16.))
                            .pt(px(12.))
                            .text_caption()
                            .text_color(c.text_subtle)
                            .child(
                                div()
                                    .font_family(buzz_ui::tokens::font::MONO)
                                    .font_weight(FontWeight::MEDIUM)
                                    .font_features(super::tabular())
                                    .child(super::video_time(time)),
                            )
                            .child(
                                Checkbox::new("comment-at-frame", self.at_frame)
                                    .label("Comment at current frame")
                                    .on_change(move |checked, _, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            this.at_frame = checked;
                                            cx.notify();
                                        });
                                    }),
                            ),
                    )
                })
                .child(div().mx(px(-4.)).child(self.composer.clone()))
                .into_any_element()
        } else {
            let text: SharedString = match state {
                ThreadState::Failed(error) => error.into(),
                ThreadState::Missing => "Original message unavailable.".into(),
                _ => "Loading comments…".into(),
            };
            div()
                .px(px(16.))
                .py(px(32.))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
                .into_any_element()
        };
        let _ = window;
        div()
            .id("media-comments")
            .flex()
            .flex_col()
            .size_full()
            .bg(c.surface_panel)
            .text_body_sm()
            .text_color(c.text_standard)
            .child(div().mb(px(8.)).child(header))
            .child(body)
    }
}
