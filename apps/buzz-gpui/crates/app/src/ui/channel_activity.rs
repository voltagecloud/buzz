//! The sidebar row's activity popover (`ChannelActivityPopover.tsx`):
//! resting on a row with unread threads or working agents opens, beside
//! it, the agents working now and the unread threads, each thread with
//! its own Mark as read.

use super::agent_activity::row::AgentActivityRow;
use super::message::display_name;
use super::{media, navigation};
use crate::agent_activity::current::{working_details, working_duration};
use crate::agent_activity::observer::{self, now_ms};
use crate::community::Community;
use buzz_relay_client::{
    EventId, Freshness, Media, Profiles, ThreadActivityItem, ThreadActivitySnapshot, message_link,
};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Icon, IconButton, IconButtonSize, IconName, Popover,
    PopoverPadding, PopoverWidth, TypeStyled, tokens::radius,
};
use gpui_kit::*;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// The popover opens after the pointer rests this long on the row, and
/// closes this long after it leaves (`delay={250} closeDelay={150}`).
const OPEN_DELAY: Duration = Duration::from_millis(250);
const CLOSE_DELAY: Duration = Duration::from_millis(150);

/// "now", "5m", "2h", "3d" (`elapsed`).
pub fn elapsed(created_at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(created_at);
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// "Workflow · owned by {name}" names trusted workflow output by its owner.
fn sender(item: &ThreadActivityItem, profiles: &Profiles) -> SharedString {
    match &item.workflow_owner_id {
        Some(owner) => super::workflows::workflow_label(&display_name(profiles, owner)).into(),
        None => display_name(profiles, &item.author_id),
    }
}

/// Opens something for `(channel, thread root or agent)`.
pub type OpenFor = Rc<dyn Fn(&str, &str, &mut Window, &mut App)>;
/// Opens a working agent: `(channel, agent, its captured message)`.
pub type OpenAgent = Rc<dyn Fn(&str, &str, Option<&str>, &mut Window, &mut App)>;

/// Where the popover's entries open, for a sidebar that does not open them
/// in Messages (Me's rows).
#[derive(Clone)]
pub struct Opens {
    /// An unread thread: `(channel, root)`.
    pub thread: OpenFor,
    pub agent: OpenAgent,
    /// An agent's activity: `(channel, agent)`.
    pub activity: OpenFor,
}

/// A sidebar row with its activity popover.
#[derive(IntoElement)]
pub struct ActivityRow {
    channel: String,
    row: AnyElement,
    activity: Option<Arc<ThreadActivitySnapshot>>,
    /// The viewer's agents working in the channel.
    agents: Vec<String>,
    profiles: Profiles,
    media: Option<Media>,
    community: Option<Entity<Community>>,
    owned: Arc<HashSet<String>>,
    opens: Option<Opens>,
}

impl ActivityRow {
    pub fn new(channel: &str, row: impl IntoElement) -> Self {
        Self {
            channel: channel.to_string(),
            row: row.into_any_element(),
            activity: None,
            agents: Vec::new(),
            profiles: Profiles::default(),
            media: None,
            community: None,
            owned: Arc::default(),
            opens: None,
        }
    }

    /// Opens the popover's entries here instead of in Messages.
    pub fn opens(mut self, opens: Opens) -> Self {
        self.opens = Some(opens);
        self
    }

    pub fn activity(
        mut self,
        activity: Option<Arc<ThreadActivitySnapshot>>,
        agents: Vec<String>,
        owned: Arc<HashSet<String>>,
    ) -> Self {
        self.activity = activity;
        self.agents = agents;
        self.owned = owned;
        self
    }

    pub fn directory(
        mut self,
        profiles: Profiles,
        media: Option<Media>,
        community: Option<Entity<Community>>,
    ) -> Self {
        self.profiles = profiles;
        self.media = media;
        self.community = community;
        self
    }

    fn items(&self) -> &[ThreadActivityItem] {
        self.activity
            .as_ref()
            .and_then(|activity| activity.items.as_deref())
            .unwrap_or_default()
    }
}

/// Whether the pointer rests on the row or the popover, and what each
/// thread's Mark as read is doing.
#[derive(Default)]
struct State {
    open: bool,
    hovered: (bool, bool),
    marking: HashSet<String>,
    failed: HashSet<String>,
    _pending: Option<Task<()>>,
    /// Redraws once a second while agents show, for their durations.
    _ticker: Option<Task<()>>,
}

impl State {
    fn hover(&mut self, row: bool, hovered: bool, active: bool, cx: &mut Context<Self>) {
        if row {
            self.hovered.0 = hovered;
        } else {
            self.hovered.1 = hovered;
        }
        let inside = (self.hovered.0 || self.hovered.1) && active;
        if inside == self.open {
            self._pending = None;
            return;
        }
        let delay = if inside { OPEN_DELAY } else { CLOSE_DELAY };
        self._pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                let inside = this.hovered.0 || this.hovered.1;
                if this.open != inside {
                    this.open = inside;
                    cx.notify();
                }
            });
        }));
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self._pending = None;
        self._ticker = None;
        self.open = false;
        cx.notify();
    }

    /// Keeps "Working now" durations current while it shows.
    fn tick(&mut self, ticking: bool, cx: &mut Context<Self>) {
        if !ticking {
            self._ticker = None;
        } else if self._ticker.is_none() {
            self._ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            }));
        }
    }
}

impl RenderOnce for ActivityRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(
            SharedString::from(format!("channel-activity-{}", self.channel)),
            cx,
            |_, _| State::default(),
        );
        let active = !self.items().is_empty() || !self.agents.is_empty();
        if !active && state.read(cx).open {
            state.update(cx, |state, cx| state.close(cx));
        }
        let open = state.read(cx).open && active;
        let ticking = open && !self.agents.is_empty();
        state.update(cx, |state, cx| state.tick(ticking, cx));
        let popover = open.then(|| popover(&self, &state, cx).into_any_element());
        let hover_state = state.clone();
        let mut row = div()
            .id(SharedString::from(format!("activity-row-{}", self.channel)))
            .relative()
            .on_hover(move |hovered, _, cx| {
                hover_state.update(cx, |state, cx| state.hover(true, *hovered, active, cx))
            })
            // A click selects the channel; it never opens the popover.
            .on_mouse_down(MouseButton::Left, {
                let state = state.clone();
                move |_, _, cx| state.update(cx, |state, cx| state.close(cx))
            })
            .child(self.row);
        if let Some(popover) = popover {
            let popover_state = state.clone();
            row = row.child(
                div().absolute().top_0().right_0().child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(6.), px(0.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "activity-popover-{}",
                                        self.channel
                                    )))
                                    .occlude()
                                    .on_hover(move |hovered, _, cx| {
                                        popover_state.update(cx, |state, cx| {
                                            state.hover(false, *hovered, active, cx)
                                        })
                                    })
                                    .child(popover),
                            ),
                    )
                    .with_priority(2),
                ),
            );
        }
        row
    }
}

fn section(label: &'static str, cx: &App) -> Div {
    let c = cx.colors();
    div()
        .flex()
        .items_center()
        .min_h(px(28.))
        .px(px(8.))
        .py(px(2.))
        .text_body_sm()
        .text_color(c.text_metadata)
        .child(label)
}

fn avatar(
    name: SharedString,
    picture: Option<&str>,
    squircle: bool,
    media: Option<&Media>,
) -> Avatar {
    let mut avatar = Avatar::new(name).size(AvatarSize::Px(30.));
    if squircle {
        avatar = avatar.squircle();
    }
    if let (Some(picture), Some(media)) = (picture, media) {
        avatar = avatar.image(media::avatar(picture, media));
    }
    avatar
}

fn popover(row: &ActivityRow, state: &Entity<State>, cx: &mut App) -> impl IntoElement {
    let c = *cx.colors();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let items = row.items();
    let stale = !items.is_empty()
        && row
            .activity
            .as_ref()
            .is_some_and(|activity| activity.freshness == Freshness::Stale);
    let channel = row.channel.clone();
    let opens = row.opens.clone();
    let open = move |root: Option<String>, window: &mut Window, cx: &mut App| {
        if let (Some(opens), Some(root)) = (&opens, &root) {
            return (opens.thread)(&channel, root, window, cx);
        }
        let url = match root.and_then(|root| EventId::from_hex(&root).ok()) {
            Some(root) => format!("{}&thread={}", message_link(&channel, &root), root.to_hex()),
            None => format!("buzz://channel/{channel}"),
        };
        navigation::open_url(&url, window, cx);
    };
    let mut list = div()
        .id(SharedString::from(format!("activity-list-{}", row.channel)))
        .flex()
        .flex_col()
        .max_h(px(360.))
        .overflow_y_scroll();
    if stale {
        list = list.child(
            div()
                .px(px(12.))
                .py(px(6.))
                .text_caption()
                .text_color(c.text_subtle)
                .child("May be out of date"),
        );
    }
    if !row.agents.is_empty() {
        list = list.child(section("Working now", cx));
        let activity = row
            .community
            .as_ref()
            .map(|community| observer::snapshot(community, cx))
            .unwrap_or_default();
        let now = now_ms();
        let channel_name = row
            .community
            .as_ref()
            .and_then(|community| {
                community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .find(|channel| channel.id == row.channel)
                    .map(|channel| channel.name.clone())
            })
            .unwrap_or_default();
        for agent in &row.agents {
            let name = display_name(&row.profiles, agent);
            let picture = row.profiles.get(agent).and_then(|p| p.picture.clone());
            // A current turn's start gives its age; only one captured
            // trigger gives a destination (`workingAgentDetails`).
            let details = working_details(&activity, &row.channel, agent);
            let message = details.as_ref().and_then(|d| d.message_id.clone());
            // Local work has its own line and no telemetry to open.
            let local = details.as_ref().is_some_and(|d| d.local);
            let status: SharedString = details
                .as_ref()
                .and_then(|d| d.label.clone())
                .unwrap_or_else(|| "Working".into())
                .into();
            let (open, open_state, view_state) = (open.clone(), state.clone(), state.clone());
            let (agent_key, channel) = (agent.clone(), row.channel.clone());
            let opened = message.clone();
            let (opens, open_agent, open_channel) =
                (row.opens.clone(), agent.clone(), row.channel.clone());
            let activity_opens = row.opens.clone();
            list = list.child(AgentActivityRow {
                id: SharedString::from(format!("working-{agent}")),
                name: name.clone(),
                picture: match (picture, row.media.as_ref()) {
                    (Some(picture), Some(media)) => Some(media::avatar(&picture, media)),
                    _ => None,
                },
                status,
                meta: Some(
                    details
                        .as_ref()
                        .map_or("—".into(), |d| working_duration(d.started_at, now))
                        .into(),
                ),
                scope: None,
                open_label: format!(
                    "Open {} for {name} in {}",
                    if message.is_some() {
                        "thread"
                    } else {
                        "conversation"
                    },
                    channel_name
                )
                .into(),
                on_open: Some(Rc::new(move |window, cx| {
                    open_state.update(cx, |state, cx| state.close(cx));
                    match &opens {
                        Some(opens) => {
                            (opens.agent)(&open_channel, &open_agent, opened.as_deref(), window, cx)
                        }
                        None => open(opened.clone(), window, cx),
                    }
                })),
                // Activity opens beside the channel it is about.
                on_open_activity: (!local).then(|| -> super::agent_activity::row::Action {
                    Rc::new(move |window, cx| {
                        view_state.update(cx, |state, cx| state.close(cx));
                        if let Some(opens) = &activity_opens {
                            return (opens.activity)(&channel, &agent_key, window, cx);
                        }
                        navigation::open_url(&format!("buzz://channel/{channel}"), window, cx);
                        super::agent_activity::open(&agent_key, Some(&channel), None, window, cx);
                    })
                }),
            });
        }
    }
    if !items.is_empty() {
        list = list.child(section("Unread threads", cx));
        let snapshot = state.read(cx);
        let (marking, failed): (HashSet<String>, HashSet<String>) =
            (snapshot.marking.clone(), snapshot.failed.clone());
        for item in items {
            let name = sender(item, &row.profiles);
            let shown = item.workflow_owner_id.as_ref().unwrap_or(&item.author_id);
            let profile = row.profiles.get(shown);
            let agent = row.owned.contains(&item.author_id)
                || row.agents.contains(&item.author_id)
                || profile.is_some_and(|p| p.is_agent);
            let picture = profile.and_then(|p| p.picture.clone());
            let root = item.root_id.clone();
            let group = SharedString::from(format!("activity-item-{root}"));
            let (open, open_state) = (open.clone(), state.clone());
            let opened = root.clone();
            let mut entry = div()
                .id(group.clone())
                .group(group.clone())
                .relative()
                .flex()
                .items_center()
                .gap(px(12.))
                .p(px(8.))
                .my(px(2.))
                .rounded(radius::FLOATING_INNER)
                .cursor_pointer()
                .hover(|s| s.bg(c.affordance_floating_hover))
                .child(match &item.workflow_owner_id {
                    Some(_) => div()
                        .flex_none()
                        .size(px(30.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(9.))
                        .bg(c.affordance_subtle)
                        .child(Icon::new(IconName::Lightning).size(20.))
                        .into_any_element(),
                    None => avatar(name.clone(), picture.as_deref(), agent, row.media.as_ref())
                        .into_any_element(),
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_baseline()
                                .justify_between()
                                .gap(px(12.))
                                .text_body_sm()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(c.text_standard)
                                        .truncate()
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .text_caption()
                                        .text_color(c.text_subtle)
                                        .group_hover(group.clone(), |s| s.invisible())
                                        .child(elapsed(item.created_at, now)),
                                ),
                        )
                        .child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_standard)
                                .line_clamp(2)
                                .child(item.preview.clone()),
                        ),
                )
                .on_click(move |_, window, cx| {
                    open_state.update(cx, |state, cx| state.close(cx));
                    open(Some(opened.clone()), window, cx);
                });
            // Mark as read, over the timestamp while the row is hovered.
            let mark = {
                let state = state.clone();
                let community = row.community.clone();
                let (channel, root, latest) = (
                    row.channel.clone(),
                    item.root_id.clone(),
                    item.latest_message_id.clone(),
                );
                IconButton::new(
                    SharedString::from(format!("activity-read-{}", item.root_id)),
                    IconName::EnvelopeOpen,
                )
                .size(IconButtonSize::Sm)
                .tooltip("Mark as read")
                .disabled(marking.contains(&item.root_id))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    mark_read(&community, &state, &channel, &root, &latest, cx);
                })
            };
            entry = entry.child(
                div()
                    .absolute()
                    .top(px(6.))
                    .right(px(4.))
                    .p(px(2.))
                    .rounded(px(8.))
                    .bg(c.affordance_subtle_hover)
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .child(mark),
            );
            list = list.child(entry);
            if failed.contains(&item.root_id) {
                list = list.child(
                    div()
                        .px(px(8.))
                        .py(px(4.))
                        .text_caption()
                        .text_color(c.text_danger)
                        .child("Could not mark this thread as read. Try again."),
                );
            }
        }
    }
    Popover::new()
        .width(PopoverWidth::Wide)
        .padding(PopoverPadding::List)
        .child(list)
}

/// Reads one thread through its newest unread reply (`markThrough`).
fn mark_read(
    community: &Option<Entity<Community>>,
    state: &Entity<State>,
    channel: &str,
    root: &str,
    latest: &str,
    cx: &mut App,
) {
    let Some(community) = community else {
        return;
    };
    let (Ok(root_id), Ok(latest)) = (EventId::from_hex(root), EventId::from_hex(latest)) else {
        return;
    };
    let root = root.to_string();
    let session = community.read(cx).session.clone();
    let channel = channel.to_string();
    state.update(cx, |state, cx| {
        state.marking.insert(root.clone());
        state.failed.remove(&root);
        cx.notify();
    });
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        let result = session.mark_through(&channel, Some(root_id), latest).await;
        let _ = state.update(cx, |state, cx| {
            state.marking.remove(&root);
            if result.is_err() {
                state.failed.insert(root);
            }
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn elapsed_times_follow_the_reference() {
        assert_eq!(elapsed(1000, 1030), "now");
        assert_eq!(elapsed(1000, 1000 + 5 * 60), "5m");
        assert_eq!(elapsed(1000, 1000 + 2 * 3600), "2h");
        assert_eq!(elapsed(1000, 1000 + 3 * 86400), "3d");
        assert_eq!(elapsed(2000, 1000), "now");
    }
}
