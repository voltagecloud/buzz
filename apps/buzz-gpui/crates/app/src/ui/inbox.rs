//! The Inbox plugin (`bundled/inbox`): mentions, direct messages and
//! replies in followed threads, with type, sender and status filters, an
//! unread toggle, a detail pane that previews the conversation, archiving
//! (see [`archive`]), and the viewer's saved drafts. Filter choices and
//! archives are saved on this device per account and community.

mod archive;
mod detail;
mod drafts;
mod model;

use super::identity::{self, AgentLibrary};
use super::overlay::{self, Align, Dismiss};
use super::pages::{self, Page, PageSpec};
use crate::community::{AppState, Community};
use crate::ui::keys::format_public_key;
use buzz_relay_client::unread::{InboxItem, InboxStatus};
use buzz_relay_client::{
    ChannelSummary, ChannelType, Freshness, InboxFeedStatus, InboxState, RosterStatus,
    SessionStatus,
};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, ButtonSize, IconButton, IconButtonSize, IconName,
    Menu, MenuItem, MenuLabel, MenuSeparator, PanelHeader, Surface, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use model::{Filters, Kind, Sender, Show, View};
use std::rc::Rc;
use std::sync::Arc;

pub const PAGE: Page = Page::new("buzz.inbox/inbox");
const FAILED: &str = "Inbox action failed. Try again.";
/// Rows whose senders' profiles are asked for.
const PROFILE_DEMAND: usize = 50;

pub fn register(cx: &mut App) {
    pages::register(
        PageSpec::sidebar(PAGE, "Inbox", IconName::Bell, |window, cx| {
            cx.new(|cx| InboxPage::new(window, cx)).into()
        }),
        cx,
    );
}

/// The visit a selected row stands for; a regrouped row keeps it while it
/// still holds the message.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Selection {
    channel_id: String,
    message_id: String,
    root_id: Option<String>,
}

/// A failed action Retry may run again while its row is unchanged.
#[derive(Clone)]
enum Failed {
    /// Reading the row, or marking it unread.
    Read { item: InboxItem, unread: bool },
    /// Archiving or restoring it; Retry keeps the click's cutoff.
    Archive {
        item: InboxItem,
        archived: bool,
        navigate: bool,
        at: u64,
    },
}

/// Why a guarded view-state write did not happen.
enum Replace {
    /// Another write came first.
    Changed,
    Failed,
}

pub struct InboxPage {
    community: Option<Entity<Community>>,
    state: Arc<InboxState>,
    view: View,
    show: Show,
    sender: Sender,
    unread_only: bool,
    /// The saved archives as last read or written, and their index.
    archives: Option<String>,
    archive_index: archive::Index,
    /// The rows as last shown, for moving on from an archived one.
    rendered: Vec<InboxItem>,
    /// The inbox state the archives were last reconciled with.
    reconciled: Option<Arc<InboxState>>,
    selected: Option<Selection>,
    detail: Option<Entity<detail::InboxDetail>>,
    drafts: drafts::State,
    pending: bool,
    error: Option<SharedString>,
    failed: Option<Failed>,
    filters_open: bool,
    /// The row whose context menu is open, and where.
    menu: Option<(String, Point<Pixels>)>,
    _watch: Option<Task<()>>,
    _community: Option<Subscription>,
}

impl InboxPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_global_in::<AppState>(window, |this, _, cx| this.bind(cx))
            .detach();
        let mut page = Self {
            community: None,
            state: Arc::new(InboxState::default()),
            view: View::default(),
            show: Show::default(),
            sender: Sender::default(),
            unread_only: false,
            archives: None,
            archive_index: archive::Index::default(),
            rendered: Vec::new(),
            reconciled: None,
            selected: None,
            detail: None,
            drafts: drafts::State::default(),
            pending: false,
            error: None,
            failed: None,
            filters_open: false,
            menu: None,
            _watch: None,
            _community: None,
        };
        page.bind(cx);
        page
    }

    /// Follows the selected community; a new one starts a fresh view.
    fn bind(&mut self, cx: &mut Context<Self>) {
        let current = AppState::current(cx);
        if current == self.community {
            return;
        }
        self.community = current.clone();
        self.state = Arc::new(InboxState::default());
        self.view = View::default();
        self.show = Show::default();
        self.sender = Sender::default();
        self.unread_only = false;
        self.archives = None;
        self.archive_index = archive::Index::default();
        self.rendered = Vec::new();
        self.reconciled = None;
        self.selected = None;
        self.detail = None;
        self.drafts = drafts::State::default();
        self.pending = false;
        self.error = None;
        self.failed = None;
        self.filters_open = false;
        self.menu = None;
        self._watch = None;
        self._community = None;
        if let Some(community) = current {
            let filters = Filters::read(self.saved(model::FILTERS_KEY, cx).as_deref());
            self.view = filters.activity;
            self.show = filters.show;
            self.sender = filters.sender;
            self.unread_only = filters.unread_only;
            self.load_archives(cx);
            let receiver = community.read(cx).session.inbox();
            self._watch = Some(crate::mirror::mirror(
                receiver,
                cx,
                |this: &mut Self, state| {
                    this.state = state;
                },
            ));
            self._community = Some(cx.observe(&community, |this, _, cx| {
                this.check_selection(cx);
                cx.notify();
            }));
        }
        cx.notify();
    }

    /// A view-state value saved for this account and community.
    fn saved(&self, key: &str, cx: &App) -> Option<String> {
        let community = self.community.as_ref()?;
        let partition = community.read(cx).partition();
        cx.global::<AppState>()
            .store
            .view_state(&partition, key)
            .ok()
            .flatten()
    }

    /// Saves `value` if the stored one is still `expected`.
    fn replace(
        &self,
        key: &str,
        expected: Option<&str>,
        value: &str,
        cx: &App,
    ) -> Result<(), Replace> {
        let Some(community) = self.community.as_ref() else {
            return Err(Replace::Failed);
        };
        let partition = community.read(cx).partition();
        let store = &cx.global::<AppState>().store;
        let current = store
            .view_state(&partition, key)
            .map_err(|_| Replace::Failed)?;
        if current.as_deref() != expected {
            return Err(Replace::Changed);
        }
        store
            .set_view_state(&partition, key, Some(value))
            .map_err(|_| Replace::Failed)
    }

    fn save_filters(&self, cx: &App) {
        let filters = Filters {
            show: self.show,
            activity: self.view,
            sender: self.sender,
            unread_only: self.unread_only,
        };
        let saved = self.saved(model::FILTERS_KEY, cx);
        if self
            .replace(model::FILTERS_KEY, saved.as_deref(), &filters.to_json(), cx)
            .is_err()
        {
            tracing::warn!("could not save the Inbox filters");
        }
    }

    fn load_archives(&mut self, cx: &App) {
        let revision = self.saved(archive::KEY, cx);
        if revision != self.archives || self.archive_index.is_empty() {
            self.archive_index = archive::Index::new(archive::read(revision.as_deref()));
            self.archives = revision;
        }
    }

    fn archived(&self, item: &InboxItem) -> bool {
        archive::is_archived(&self.archive_index, item, self.view == View::Mentions)
    }

    /// Saves reply reopening and retires archives a fresh mention renewed
    /// (`reopenArchives`). A newer saved revision is read instead.
    fn reopen(&mut self, cx: &mut Context<Self>) -> Result<(), &'static str> {
        let revision = self.archives.clone();
        let Some(next) = archive::reopened(revision.as_deref(), &self.state.inbox.items) else {
            return Ok(());
        };
        let result = self.replace(archive::KEY, revision.as_deref(), &next, cx);
        self.load_archives(cx);
        cx.notify();
        match result {
            Err(Replace::Failed) => Err(archive::REOPEN_UNSAVED),
            _ => Ok(()),
        }
    }

    /// Archives or restores a conversation (`archive`). Archiving the open
    /// conversation moves on to the next row shown (or the one before);
    /// with none left the detail closes. Restore keeps the reader open.
    fn archive(
        &mut self,
        item: InboxItem,
        archived: bool,
        navigate: bool,
        at: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending {
            return;
        }
        self.failed = None;
        self.menu = None;
        let at = at.unwrap_or_else(now_seconds);
        let same = |row: &InboxItem| {
            row.channel_id == item.channel_id && row.message_ids.contains(&item.message_id)
        };
        if !self.state.inbox.items.iter().any(same) {
            self.error = Some(buzz_relay_client::session::INBOX_EXPIRED.into());
            cx.notify();
            return;
        }
        self.load_archives(cx);
        let leaves = match self.show {
            Show::Inbox => archived,
            Show::Archived => !archived,
        };
        let next = self
            .rendered
            .iter()
            .position(same)
            .filter(|_| leaves)
            .and_then(|index| {
                self.rendered
                    .get(index + 1)
                    .or_else(|| index.checked_sub(1).and_then(|i| self.rendered.get(i)))
            })
            .cloned();
        let open = self.selected_item().is_some_and(same);
        let revision = self.archives.clone();
        let saved = archive::updated(revision.as_deref(), &item, archived, at).and_then(|json| {
            self.replace(archive::KEY, revision.as_deref(), &json, cx)
                .map_err(|error| match error {
                    Replace::Changed => archive::CHANGED,
                    Replace::Failed => archive::UNSAVED,
                })
        });
        self.load_archives(cx);
        match saved {
            Err(error) => {
                self.error = Some(error.into());
                self.failed = Some(Failed::Archive {
                    item,
                    archived,
                    navigate,
                    at,
                });
            }
            Ok(()) => {
                self.error = None;
                if leaves && navigate && open {
                    match next {
                        Some(next) => self.select(&next, window, cx),
                        None => self.close_detail(cx),
                    }
                }
            }
        }
        cx.notify();
    }

    fn channels(&self, cx: &App) -> Arc<[ChannelSummary]> {
        self.community
            .as_ref()
            .map(|community| community.read(cx).snapshot.channels.clone())
            .unwrap_or_else(|| Arc::new([]))
    }

    fn selected_item(&self) -> Option<&InboxItem> {
        let selected = self.selected.as_ref()?;
        self.state.inbox.items.iter().find(|item| {
            item.channel_id == selected.channel_id
                && item.message_ids.contains(&selected.message_id)
        })
    }

    /// The selected row is gone (once loaded) or its channel archived:
    /// close the detail.
    fn check_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let archived = self
            .channels(cx)
            .iter()
            .any(|channel| channel.id == selected.channel_id && channel.archived);
        let gone =
            self.selected_item().is_none() && self.state.inbox.status != InboxStatus::Loading;
        if archived || gone {
            if archived {
                self.failed = None;
                self.error = None;
            }
            self.close_detail(cx);
        }
    }

    fn close_detail(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.detail = None;
        self.failed = None;
        self.error = None;
        cx.notify();
    }

    fn select(&mut self, item: &InboxItem, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending || self.selected_item().is_some_and(|row| row.id == item.id) {
            return;
        }
        let Some(community) = self.community.clone() else {
            return;
        };
        self.failed = None;
        self.error = None;
        let selection = Selection {
            channel_id: item.channel_id.clone(),
            message_id: item.message_id.clone(),
            root_id: item.root_id.clone(),
        };
        self.selected = Some(selection);
        let incomplete = self.preview_incomplete(item);
        self.detail =
            Some(cx.new(|cx| {
                detail::InboxDetail::new(community, item.clone(), incomplete, window, cx)
            }));
        let page = cx.entity().downgrade();
        if let Some(detail) = &self.detail {
            cx.subscribe(detail, move |_, _, _: &detail::Closed, cx| {
                let _ = page.update(cx, |this, cx| this.close_detail(cx));
            })
            .detach();
            cx.subscribe_in(
                detail,
                window,
                |this, _, _: &detail::ToggleArchive, window, cx| {
                    if let Some(item) = this.selected_item().cloned() {
                        let archived = !this.archived(&item);
                        this.archive(item, archived, archived, None, window, cx);
                    }
                },
            )
            .detach();
        }
        if model::has_unread(item) && self.state.can_read {
            self.mutate(item.clone(), false, cx);
        }
        cx.notify();
    }

    /// Reads a row, or marks it unread on this device.
    fn mutate(&mut self, item: InboxItem, unread: bool, cx: &mut Context<Self>) {
        let Some(community) = self.community.clone() else {
            return;
        };
        if self.pending || !self.state.can_read {
            return;
        }
        self.pending = true;
        self.failed = None;
        self.menu = None;
        let session = community.read(cx).session.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = if unread {
                session.mark_inbox_item_unread(item.clone()).await
            } else {
                session.read_inbox_item(item.clone()).await
            };
            let _ = this.update(cx, |this, cx| {
                this.pending = false;
                match result {
                    Ok(()) => this.error = None,
                    Err(error) => {
                        // Retry runs it again only while the row is current.
                        if error != buzz_relay_client::session::INBOX_EXPIRED {
                            this.failed = Some(Failed::Read { item, unread });
                        }
                        this.error = Some(if error.is_empty() {
                            FAILED.into()
                        } else {
                            error.into()
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh(&mut self, retry_sync: bool, cx: &mut Context<Self>) {
        let Some(community) = self.community.clone() else {
            return;
        };
        self.load_archives(cx);
        if let Err(error) = self.reopen(cx) {
            self.error = Some(error.into());
        }
        let session = &community.read(cx).session;
        session.refresh_inbox();
        if retry_sync {
            session.retry_read_sync();
        }
        cx.notify();
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.failed.take() {
            Some(Failed::Read { item, unread }) => {
                self.error = None;
                self.mutate(item, unread, cx);
            }
            Some(Failed::Archive {
                item,
                archived,
                navigate,
                at,
            }) => {
                self.error = None;
                self.archive(item, archived, navigate, Some(at), window, cx);
            }
            None => {
                self.error = None;
                self.refresh(true, cx);
            }
        }
    }

    /// `"loading"` or `"error"` while a row's messages wait for their
    /// edits and deletions.
    fn preview_incomplete(&self, item: &InboxItem) -> Option<detail::Incomplete> {
        let feed = &self.state.feed;
        item.message_ids
            .iter()
            .any(|id| feed.incomplete.contains(id))
            .then_some(if feed.status == InboxFeedStatus::Error {
                detail::Incomplete::Error
            } else {
                detail::Incomplete::Loading
            })
    }

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.filters_open = false;
        if (view == View::Drafts) != (self.view == View::Drafts) {
            self.failed = None;
            self.error = None;
        }
        let entering = view == View::Drafts && self.view != View::Drafts;
        self.view = view;
        if entering {
            self.open_drafts(cx);
        } else if view != View::Drafts {
            self.drafts = drafts::State::default();
            self.save_filters(cx);
        }
        cx.notify();
    }

    /// Inbox or Archived; the open conversation closes.
    fn set_show(&mut self, show: Show, cx: &mut Context<Self>) {
        self.filters_open = false;
        if show == self.show {
            cx.notify();
            return;
        }
        self.failed = None;
        self.selected = None;
        self.detail = None;
        self.show = show;
        self.save_filters(cx);
        cx.notify();
    }

    fn sender_kind(&self, author: &str, names: &identity::Names) -> Kind {
        if names.is_agent(author) {
            return Kind::Agent;
        }
        match names.profiles().get(author) {
            Some(profile) if !profile.name_is_fallback => Kind::Human,
            _ => Kind::Unknown,
        }
    }

    fn toolbar(&self, loading: bool, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let drafts = self.view == View::Drafts;
        let page = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = page.update(cx, |this, cx| {
                this.filters_open = false;
                cx.notify();
            });
        });
        let sender = self.sender;
        let menu = Menu::new()
            .tall()
            .child(MenuLabel::new("Type").quiet())
            .children(View::TYPES.into_iter().map(|view| {
                MenuItem::new(("inbox-type", view as usize), view.label())
                    .highlighted(self.view == view)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_view(view, cx)))
            }))
            .child(MenuSeparator::new())
            .child(MenuLabel::new("From").quiet())
            .child(
                MenuItem::new("inbox-from-people", "People")
                    .checked(sender != Sender::Agents)
                    .disabled(drafts || sender == Sender::Humans)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sender =
                            model::toggle_sender(this.sender, true, sender == Sender::Agents);
                        this.save_filters(cx);
                        cx.notify();
                    })),
            )
            .child(
                MenuItem::new("inbox-from-agents", "Agents")
                    .checked(sender != Sender::Humans)
                    .disabled(drafts || sender == Sender::Agents)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sender =
                            model::toggle_sender(this.sender, false, sender == Sender::Humans);
                        this.save_filters(cx);
                        cx.notify();
                    })),
            )
            .child(MenuSeparator::new())
            .child(MenuLabel::new("Status").quiet())
            .children([(Show::Inbox, "Inbox"), (Show::Archived, "Archived")].map(
                |(show, label)| {
                    MenuItem::new(("inbox-status", show as usize), label)
                        .highlighted(self.show == show)
                        .disabled(drafts)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_show(show, cx)))
                },
            ))
            .child(MenuSeparator::new())
            .child(
                MenuItem::new("inbox-drafts", View::Drafts.label())
                    .highlighted(drafts)
                    .on_click(cx.listener(|this, _, _, cx| this.set_view(View::Drafts, cx))),
            );
        div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(space::S1)
            .min_h(rems(2.5))
            .px(space::S1)
            .border_b_1()
            .border_color(c.border_standard)
            .child(
                div()
                    .relative()
                    .child(
                        Button::new(
                            "inbox-filters",
                            model::summary(self.view, self.show, self.sender),
                        )
                        .size(ButtonSize::Sm)
                        .ghost()
                        .icon_end(IconName::CaretDown)
                        .selected(self.filters_open)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.filters_open = !this.filters_open;
                            cx.notify();
                        })),
                    )
                    .when(self.filters_open, |this| {
                        this.child(overlay::dropdown(window, Align::Start, menu, dismiss))
                    }),
            )
            .when(!drafts && loading, |this| {
                this.child(
                    div().flex().flex_1().min_w_0().items_center().child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_caption()
                            .text_color(c.text_subtle)
                            .child("Checking recent activity…"),
                    ),
                )
            })
            .when(!drafts, |this| {
                this.child(
                    IconButton::new(
                        "inbox-unread-only",
                        if self.unread_only {
                            IconName::NotificationFilled
                        } else {
                            IconName::Notification
                        },
                    )
                    .size(IconButtonSize::Sm)
                    .icon_size(20.)
                    .selected(false)
                    .tooltip("Show unread only")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.unread_only = !this.unread_only;
                        this.save_filters(cx);
                        cx.notify();
                    })),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        item: &InboxItem,
        channel: Option<&ChannelSummary>,
        names: &identity::Names,
        media: &buzz_relay_client::Media,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let ix = c.interaction(Surface::Panel);
        let display = item.workflow_owner_id.as_deref().unwrap_or(&item.author_id);
        let fallback = format_public_key(display).unwrap_or_else(|| "Unknown sender".into());
        let resolved = names.name(display, &fallback);
        let sender = match item.workflow_owner_id {
            Some(_) => format!("Workflow · owned by {resolved}"),
            None => resolved,
        };
        let dm = channel.is_some_and(|channel| channel.channel_type == Some(ChannelType::Dm));
        let context = match channel {
            Some(channel) if dm => format!(
                "DM · {}",
                model::dm_label(
                    channel.participants.as_deref(),
                    |key, fallback| names.name(key, fallback),
                    if channel.name.is_empty() {
                        "Direct message"
                    } else {
                        &channel.name
                    },
                )
            ),
            Some(channel) => format!("#{}", channel.name),
            None => "Conversation".into(),
        };
        let unread = model::has_unread(item);
        let selected = self.selected_item().is_some_and(|row| row.id == item.id);
        let profile = names.profiles().get(display);
        let agent = item.workflow_owner_id.is_some() || profile.is_some_and(|p| p.is_agent);
        let mut avatar = Avatar::new(sender.clone()).size(AvatarSize::Default);
        if agent {
            avatar = avatar.squircle();
        }
        if item.workflow_owner_id.is_some() {
            avatar = avatar.fallback_icon(IconName::Lightning, 20.);
        } else if let Some(picture) = profile.and_then(|p| p.picture.clone()) {
            avatar = avatar.image(super::media::image(&picture, media));
        }
        let preview: String = if item
            .message_ids
            .iter()
            .any(|id| self.state.feed.incomplete.contains(id))
        {
            if self.state.feed.status == InboxFeedStatus::Error {
                "Preview unavailable. Retry inbox.".into()
            } else {
                "Preview updating…".into()
            }
        } else {
            message_preview(&item.preview)
        };
        let click_item = item.clone();
        let menu_id = item.id.clone();
        let archived = self.archived(item);
        let archive_item = item.clone();
        let group = SharedString::from(format!("inbox-row-{}", item.id));
        div()
            .id(group.clone())
            .group(group.clone())
            .flex()
            .items_center()
            .gap(space::ROW_GAP)
            .min_h(rems(2.))
            .px(rems(1.))
            .py(rems(0.25))
            .rounded(radius::ROW)
            .text_color(c.text_standard)
            .when(selected, |this| this.bg(ix.selected))
            .when(!self.pending, |this| {
                this.cursor_pointer()
                    .when(!selected, |this| this.hover(move |s| s.bg(ix.row_hover)))
            })
            .child(div().flex_none().child(avatar))
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
                            .flex()
                            .flex_wrap()
                            .items_baseline()
                            .justify_between()
                            .gap_x(space::CONTROL_INSET)
                            .gap_y(space::S1)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_label_sm()
                                    .text_color(c.text_standard)
                                    .child(sender),
                            )
                            .child(
                                div()
                                    .text_caption()
                                    .text_color(c.text_subtle)
                                    .child(model::relative_timestamp(item.created_at, now)),
                            ),
                    )
                    .child(
                        div().flex().min_w_0().child(
                            div()
                                .flex()
                                .items_center()
                                .min_w_0()
                                .max_w_full()
                                .rounded(radius::CHIP)
                                .px(space::CHIP_INSET)
                                .bg(c.affordance_subtle)
                                .text_color(c.text_subtle)
                                .text_caption()
                                .child(div().truncate().child(context)),
                        ),
                    )
                    .child(
                        div()
                            .text_body()
                            .text_color(if unread {
                                c.text_standard
                            } else {
                                c.text_subtle
                            })
                            .line_clamp(2)
                            .child(preview),
                    ),
            )
            .child(div().flex_none().size(space::S2).when(unread, |this| {
                this.rounded_full().bg(c.text_navigation_unread)
            }))
            // Revealed on hover; a click never opens or reads the row.
            .child(
                div()
                    .flex_none()
                    .invisible()
                    .group_hover(group, |this| this.visible())
                    .child(
                        IconButton::new(
                            SharedString::from(format!("inbox-archive-{}", item.id)),
                            if archived {
                                IconName::ArchiveOff
                            } else {
                                IconName::Archive
                            },
                        )
                        .size(IconButtonSize::Sm)
                        .disabled(self.pending)
                        .tooltip(if archived { "Restore" } else { "Archive" })
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.archive(
                                    archive_item.clone(),
                                    !archived,
                                    !archived,
                                    None,
                                    window,
                                    cx,
                                )
                            },
                        )),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.select(&click_item, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn row_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (id, at) = self.menu.clone()?;
        let item = self
            .state
            .inbox
            .items
            .iter()
            .find(|item| item.id == id)?
            .clone();
        let unread = model::has_unread(&item);
        let disabled = self.pending || unread || !self.state.can_read;
        let archived = self.archived(&item);
        let archive_item = item.clone();
        let content = Menu::new()
            .size(buzz_ui::MenuSize::Compact)
            .child(
                MenuItem::new(
                    "inbox-archive",
                    if archived {
                        "Restore conversation"
                    } else {
                        "Archive conversation"
                    },
                )
                .size(buzz_ui::MenuSize::Compact)
                .disabled(self.pending)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.archive(archive_item.clone(), !archived, !archived, None, window, cx)
                })),
            )
            .child(
                MenuItem::new("inbox-mark-unread", "Mark unread")
                    .size(buzz_ui::MenuSize::Compact)
                    .disabled(disabled)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.mutate(item.clone(), true, cx)),
                    ),
            );
        let page = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = page.update(cx, |this, cx| {
                this.menu = None;
                cx.notify();
            });
        });
        Some(overlay::context_menu(window, at, content, dismiss).into_any_element())
    }

    fn notice(children: Vec<AnyElement>) -> Div {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(space::S3)
            .py(space::S3)
            .px(space::PANEL_INSET)
            .children(children)
    }
}

/// `messagePreview`: plain text, or "New message".
fn message_preview(content: &str) -> String {
    let text = crate::notifications::plain_text(content);
    if text.is_empty() {
        "New message".into()
    } else {
        text
    }
}

impl Render for InboxPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let header = || {
            PanelHeader::new()
                .joined(true)
                .label(Some(IconName::Bell), "Inbox")
        };
        let root = div()
            .id("inbox-page")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .text_color(c.text_standard);
        let Some(community) = self.community.clone() else {
            return root.child(header()).child(Self::notice(vec![
                div()
                    .text_body()
                    .text_color(c.text_subtle)
                    .child("Choose a community to see your inbox.")
                    .into_any_element(),
            ]));
        };
        let snapshot = community.read(cx).snapshot.clone();
        match &snapshot.status {
            SessionStatus::Ready => {}
            SessionStatus::Connecting => {
                return root.child(header()).child(Self::notice(vec![
                    div()
                        .text_body()
                        .text_color(c.text_subtle)
                        .child("Connecting to your inbox…")
                        .into_any_element(),
                ]));
            }
            SessionStatus::Failed(error) => {
                let session = community.read(cx).session.clone();
                return root.child(header()).child(Self::notice(vec![
                    div()
                        .text_body()
                        .text_color(c.text_subtle)
                        .child(error.clone())
                        .into_any_element(),
                    Button::new("inbox-retry-connection", "Retry connection")
                        .on_click(move |_, _, _| session.retry())
                        .into_any_element(),
                ]));
            }
        }
        // New activity may reopen archived conversations.
        if !self
            .reconciled
            .as_ref()
            .is_some_and(|seen| Arc::ptr_eq(seen, &self.state))
        {
            self.reconciled = Some(self.state.clone());
            if let Err(error) = self.reopen(cx) {
                self.error = Some(error.into());
            }
        }
        let state = self.state.clone();
        let drafts = self.view == View::Drafts;
        let roster_loading = snapshot.roster == RosterStatus::Loading;
        let loading = roster_loading
            || state.inbox.status == InboxStatus::Loading
            || state.feed.status == InboxFeedStatus::Loading;
        let failure: Option<SharedString> = self
            .error
            .clone()
            .or_else(|| match &snapshot.roster {
                RosterStatus::Error(error) => Some(error.clone().into()),
                _ => None,
            })
            .or_else(|| state.inbox.error.clone().map(Into::into))
            .or_else(|| state.feed.error.clone().map(Into::into))
            .or_else(|| {
                snapshot
                    .unread
                    .sync
                    .as_ref()
                    .and_then(|sync| sync.error.clone())
                    .map(Into::into)
            });
        let toolbar = self.toolbar(loading, window, cx);
        let failure_notice = (!drafts)
            .then_some(failure.clone())
            .flatten()
            .map(|failure| {
                Self::notice(vec![
                    div().text_body().child(failure).into_any_element(),
                    Button::new("inbox-retry", "Retry inbox")
                        .size(ButtonSize::Sm)
                        .loading(self.pending)
                        .on_click(cx.listener(|this, _, window, cx| this.retry(window, cx)))
                        .into_any_element(),
                ])
            });
        if drafts {
            let list = self.drafts_list(cx);
            let detail = self.draft_detail();
            let error = self.draft_error().filter(|_| detail.is_some());
            let dialog = self.draft_delete_dialog(window, cx);
            return root.child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(rems(20.))
                            .min_h_0()
                            .child(toolbar)
                            .child(list),
                    )
                    .children(detail.map(|detail| {
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(rems(26.25))
                            .min_w(rems(19.75))
                            .min_h_0()
                            .border_l_1()
                            .border_color(c.border_standard)
                            .children(
                                error.map(|error| {
                                    div().px(space::PANEL_INSET).text_body().child(error)
                                }),
                            )
                            .child(detail)
                    }))
                    .children(dialog),
            );
        }

        // Rows: the view filter, then the sender filter on enriched
        // authors, then Unread only (the selected row always stays).
        let channels = snapshot.channels.clone();
        let channel = |id: &str| channels.iter().find(|channel| channel.id == id);
        let library = AgentLibrary::from_hosts(cx);
        let viewer = community.read(cx).session.viewer().to_string();
        let origin = community.read(cx).origin.clone();
        let names = identity::names(&snapshot.profiles, &library, &viewer, Some(&origin), None);
        let showing_archived = self.show == Show::Archived;
        let activity: Vec<&InboxItem> = state
            .inbox
            .items
            .iter()
            .filter(|item| self.archived(item) == showing_archived)
            .filter(|item| {
                let dm = channel(&item.channel_id)
                    .is_some_and(|channel| channel.channel_type == Some(ChannelType::Dm));
                model::matches_activity(item, self.view, dm)
            })
            .collect();
        let selected_id = self.selected_item().map(|item| item.id.clone());
        let visible: Vec<InboxItem> = activity
            .iter()
            .filter(|item| match self.sender {
                Sender::Everyone => true,
                Sender::Agents => self.sender_kind(&item.author_id, &names) == Kind::Agent,
                Sender::Humans => self.sender_kind(&item.author_id, &names) == Kind::Human,
            })
            .filter(|item| {
                !self.unread_only
                    || model::has_unread(item)
                    || selected_id.as_deref() == Some(item.id.as_str())
            })
            .map(|item| (*item).clone())
            .collect();
        // Profiles for the first rows' senders, workflow owners and DM
        // people, once the roster is read.
        if !roster_loading {
            let mut people: Vec<String> = activity
                .iter()
                .take(PROFILE_DEMAND)
                .map(|item| item.author_id.clone())
                .collect();
            for item in visible.iter().take(PROFILE_DEMAND) {
                people.extend(item.workflow_owner_id.clone());
                if let Some(channel) = channel(&item.channel_id)
                    .filter(|channel| channel.channel_type == Some(ChannelType::Dm))
                {
                    people.extend(channel.participants.iter().flatten().take(3).cloned());
                }
            }
            people.sort();
            people.dedup();
            if !people.is_empty() {
                community.read(cx).session.want_profiles(people);
            }
        }
        let media = community.read(cx).session.media().clone();
        let now = now_seconds();
        let rows: Vec<AnyElement> = visible
            .iter()
            .map(|item| {
                let channel = channel(&item.channel_id);
                // Namesakes are told apart within the row's channel.
                let scoped = identity::names(
                    &snapshot.profiles,
                    &library,
                    &viewer,
                    Some(&origin),
                    channel.map(|channel| channel.members.as_slice()),
                );
                self.row(item, channel, &scoped, &media, now, cx)
            })
            .collect();
        let mut notices: Vec<AnyElement> = Vec::new();
        if state.inbox.freshness == Freshness::Stale && failure.is_none() {
            notices.push(
                Self::notice(vec![
                    div()
                        .text_body()
                        .text_color(c.text_subtle)
                        .child("Showing retained activity.")
                        .into_any_element(),
                    Button::new("inbox-refresh-stale", "Refresh")
                        .size(ButtonSize::Sm)
                        .ghost()
                        .loading(self.pending)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(false, cx)))
                        .into_any_element(),
                ])
                .into_any_element(),
            );
        }
        if (state.inbox.status == InboxStatus::Idle || state.feed.status == InboxFeedStatus::Idle)
            && !loading
            && failure.is_none()
        {
            notices.push(
                Self::notice(vec![
                    div()
                        .text_body()
                        .text_color(c.text_subtle)
                        .child("Check recent activity.")
                        .into_any_element(),
                    Button::new("inbox-refresh-idle", "Refresh")
                        .size(ButtonSize::Sm)
                        .ghost()
                        .loading(self.pending)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(false, cx)))
                        .into_any_element(),
                ])
                .into_any_element(),
            );
        }
        if visible.is_empty()
            && (state.inbox.status == InboxStatus::Ready
                || state.feed.status == InboxFeedStatus::Ready)
            && !loading
            && failure.is_none()
        {
            notices.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S1)
                    .py(space::PAGE_SECTION_GAP)
                    .px(space::PANEL_INSET)
                    .max_w(rems(40.))
                    .child(
                        div()
                            .text_label()
                            .child(match (showing_archived, self.unread_only) {
                                (true, true) => "No archived conversations match Unread only",
                                (true, false) => "No archived conversations in this view",
                                (false, true) => "No unread activity in this view",
                                (false, false) => "No recent activity in this view",
                            }),
                    )
                    .child(div().text_body().text_color(c.text_subtle).child(
                        match (showing_archived, self.view == View::Mentions) {
                            (true, true) => {
                                "Archived mentions stay here until you restore them or receive \
                                 a new mention."
                            }
                            (true, false) => {
                                "Archived conversations stay here until you restore them, \
                                 receive a new participating-thread reply, or receive a new \
                                 mention."
                            }
                            (false, _) => {
                                "Mentions, direct messages, and replies in threads you \
                                 participate in appear here."
                            }
                        },
                    ))
                    .when(showing_archived && self.unread_only, |this| {
                        this.child(
                            div().pt(space::S2).child(
                                Button::new(
                                    "inbox-show-all-archived",
                                    "Show all archived conversations",
                                )
                                .size(ButtonSize::Sm)
                                .ghost()
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.unread_only = false;
                                        this.save_filters(cx);
                                        cx.notify();
                                    },
                                )),
                            ),
                        )
                    })
                    .into_any_element(),
            );
        }
        // Keep the open detail's preview and archive state current.
        if let (Some(detail), Some(item)) = (self.detail.clone(), self.selected_item().cloned()) {
            let incomplete = self.preview_incomplete(&item);
            let archive = detail::ArchiveAction {
                archived: self.archived(&item),
                disabled: self.pending,
            };
            detail.update(cx, |detail, cx| {
                detail.update_item(item, incomplete, archive, window, cx)
            });
        }
        self.rendered = visible;
        let detail = self
            .detail
            .clone()
            .filter(|_| self.selected_item().is_some());
        let menu = self.row_menu(window, cx);
        root.children(failure_notice).child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(rems(20.))
                        .min_h_0()
                        .child(toolbar)
                        .child(
                            div()
                                .id("inbox-scroll")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .children(notices)
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .px(space::ROW_GAP)
                                        .pb(space::PANEL_INSET)
                                        .children(rows),
                                ),
                        ),
                )
                .children(detail.map(|detail| {
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .w(rems(26.25))
                        .min_w(rems(19.75))
                        .min_h_0()
                        .border_l_1()
                        .border_color(c.border_standard)
                        .child(detail)
                }))
                .children(menu),
        )
    }
}

/// The time now, in whole unix seconds.
fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
