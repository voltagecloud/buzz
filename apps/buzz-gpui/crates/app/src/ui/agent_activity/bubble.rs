//! The activity bubble on the exact message an agent is handling
//! (`ActivityAccessory`): the agents' avatars with dots that breathe while
//! they start and pulse once they work, "Could not finish" or "Status
//! unknown". Clicking it opens the message's thread, even before a reply;
//! resting on it lists each agent's latest step, which opens the agent's
//! profile on its activity. A failure can be dismissed on this device.

use crate::agent_activity::local::{self, LocalActivity, LocalWorkState, local_work_label};
use crate::agent_activity::observer;
use crate::agent_activity::request::{self, Preview, RequestWork, WorkState, message_activity};
use crate::community::Community;
use crate::ui::{identity::Names, media};
use buzz_relay_client::{EventId, Media, Profiles};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, IconButton, IconButtonSize, IconName, Popover, PopoverPadding,
    PopoverWidth, StackSize, TypeStyled, avatar_stack, shimmer_text, typing_dots,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::Arc;

/// The bubbles of one conversation's rows, by message.
#[derive(Clone)]
pub struct Bubbles {
    pub community: WeakEntity<Community>,
    pub channel: String,
    pub works: Arc<HashMap<EventId, Vec<RequestWork>>>,
}

impl Bubbles {
    /// Every message in `channel` with work to show.
    pub fn of(community: &Entity<Community>, channel: &str, cx: &App) -> Self {
        let snapshot = observer::snapshot(community, cx);
        let works = request::candidates(&snapshot, channel)
            .into_iter()
            .filter_map(|id| {
                let works = message_activity(&snapshot, channel, &id);
                (!works.is_empty()).then(|| Some((EventId::from_hex(&id).ok()?, works)))
            })
            .flatten()
            .collect();
        Self {
            community: community.downgrade(),
            channel: channel.to_string(),
            works: Arc::new(works),
        }
    }
}

/// Received, or handed over with no step yet: breathing "Starting".
fn starting(work: &RequestWork) -> bool {
    work.lifecycle
        && (work.state == WorkState::Waiting
            || (work.state == WorkState::Working && work.step.is_none()))
}

/// The line a bubble's entry reads as (`label`).
fn label(work: &RequestWork) -> String {
    if !work.lifecycle {
        return if work.working {
            "working".into()
        } else {
            "status unknown".into()
        };
    }
    let state = match work.state {
        WorkState::Waiting => LocalWorkState::Waiting,
        WorkState::Working => LocalWorkState::Working,
        WorkState::Done | WorkState::Ended => LocalWorkState::Done,
        WorkState::Superseded => LocalWorkState::Superseded,
        WorkState::Error | WorkState::Unknown => LocalWorkState::Error,
    };
    local_work_label(state, work.reason.as_deref(), work.step.as_ref())
}

/// Names for the bubble's agents: the directory's, with the key's suffix
/// when two read alike.
fn names(works: &[RequestWork], names: &Names) -> Vec<SharedString> {
    let keys = crate::ui::agent_activity::public_key_labels(works.iter().map(|w| &w.agent));
    let plain: Vec<String> = works.iter().map(|w| names.display(&w.agent)).collect();
    plain
        .iter()
        .zip(works)
        .map(|(name, work)| {
            if plain.iter().filter(|other| *other == name).count() > 1
                && let Some(suffix) = keys.get(&work.agent)
            {
                format!("{name} · {suffix}").into()
            } else {
                name.clone().into()
            }
        })
        .collect()
}

fn avatar(name: &SharedString, agent: &str, profiles: &Profiles, media_: &Media) -> Avatar {
    let mut avatar = Avatar::new(name.clone()).squircle();
    if let Some(picture) = profiles.get(agent).and_then(|p| p.picture.as_deref()) {
        avatar = avatar.image(media::avatar(picture, media_));
    }
    avatar
}

/// The bubble under a message, beside its reactions.
pub fn bubble(
    id: EventId,
    works: &[RequestWork],
    bubbles: &Bubbles,
    directory: &crate::ui::markdown::Directory,
    open_thread: Option<super::row::Action>,
    cx: &App,
) -> AnyElement {
    let c = *cx.colors();
    let hex = id.to_hex();
    let names = names(works, &directory.names);
    let working = works.iter().any(|w| w.working);
    let waiting = works.iter().any(starting);
    let stepping = works.iter().any(|w| w.working && !starting(w));
    let failed = works
        .iter()
        .any(|w| w.lifecycle && w.state == WorkState::Error);
    let unknown = works.iter().any(|w| !w.lifecycle && !w.working);
    let avatars = works
        .iter()
        .zip(&names)
        .map(|(work, name)| avatar(name, &work.agent, &directory.profiles, &directory.media))
        .collect();
    // Where links open nowhere, neither does the activity (`canOpen`).
    let closed = crate::ui::links::mode() == crate::ui::links::LinkMode::Closed;
    let popover = {
        let (community, channel) = (bubbles.community.clone(), bubbles.channel.clone());
        move |_: &mut Window, cx: &mut App| -> AnyView {
            let (community, channel) = (community.clone(), channel.clone());
            cx.new(|cx| BubblePopover::new(community, channel, id, closed, cx))
                .into()
        }
    };
    let button = div()
        .id(SharedString::from(format!("activity-bubble-{hex}")))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.))
        .h(px(24.))
        .px(px(6.))
        .rounded(px(6.))
        .text_caption()
        .text_color(c.text_standard)
        .cursor_pointer()
        .hover(|style| style.bg(c.affordance_subtle_hover))
        .children(avatar_stack(
            format!("activity-stack-{hex}"),
            avatars,
            StackSize::Compact,
            cx,
        ))
        .when(working || waiting, |d| {
            d.child(typing_dots(
                SharedString::from(format!("activity-dots-{hex}")),
                !stepping,
                c.text_standard,
                cx,
            ))
        })
        .when(failed, |d| d.child("Could not finish"))
        .when(unknown, |d| d.child("Status unknown"))
        .hoverable_tooltip(popover)
        .when_some(open_thread, |d, open| {
            d.on_click(move |_, window, cx| open(window, cx))
        });
    buzz_ui::arrive(
        SharedString::from(format!("activity-arrive-{hex}")),
        div().flex().mt(px(8.)).min_w_0().child(button),
        buzz_ui::Arrive::Grow,
        cx,
    )
}

/// The list a bubble opens on hover: each agent's latest step, live.
pub struct BubblePopover {
    community: WeakEntity<Community>,
    channel: String,
    message: EventId,
    /// Its agents' activity does not open (a reader's).
    closed: bool,
    _observe: Vec<Subscription>,
}

impl BubblePopover {
    fn new(
        community: WeakEntity<Community>,
        channel: String,
        message: EventId,
        closed: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut observe = Vec::new();
        if let Some(community) = community.upgrade() {
            if let Some(activity) = observer::get(&community, cx) {
                observe.push(cx.observe(&activity, |_, _, cx| cx.notify()));
            }
            observe.push(cx.observe(&community, |_, _, cx| cx.notify()));
        }
        Self {
            community,
            channel,
            message,
            closed,
            _observe: observe,
        }
    }
}

impl Render for BubblePopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut popover = Popover::new()
            .width(PopoverWidth::Default)
            .padding(PopoverPadding::List);
        let Some(community) = self.community.upgrade() else {
            return popover;
        };
        let c = *cx.colors();
        let snapshot = observer::snapshot(&community, cx);
        let works = message_activity(&snapshot, &self.channel, &self.message.to_hex());
        let (directory, scope) = {
            let community = community.read(cx);
            (
                crate::ui::markdown::Directory::of(community, None, cx),
                local::scope(&community.origin.http(), community.session.viewer()),
            )
        };
        let names = names(&works, &directory.names);
        for (work, name) in works.iter().zip(names) {
            let agent = work.agent.clone();
            let error = work.lifecycle && work.state == WorkState::Error;
            let preview: Option<Preview> = (work.working && !work.lifecycle)
                .then(|| {
                    let turns = crate::agent_activity::current::live_turns(
                        &snapshot,
                        &agent,
                        &self.channel,
                    );
                    request::preview(&turns, work)
                })
                .flatten();
            let line = if work.lifecycle {
                label(work)
            } else if !work.working {
                "Activity interrupted or out of date".into()
            } else {
                preview
                    .as_ref()
                    .map_or("Waiting for activity details".into(), |p| p.title.clone())
            };
            let live = if work.lifecycle {
                work.state == WorkState::Working
            } else {
                work.working
            };
            let id = format!("{}-{agent}", self.message.to_hex());
            let mut text = div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .min_w_0()
                .flex_1()
                .text_body_sm();
            text = if error {
                // Errors say what to do: readable, not clipped.
                text.child(div().text_color(c.text_standard).child(line))
            } else {
                text.child(shimmer_text(
                    SharedString::from(format!("activity-line-{id}")),
                    line,
                    live,
                    cx,
                ))
            };
            if let Some(detail) = preview.map(|p| p.detail).filter(|d| !d.is_empty()) {
                text = text.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(c.text_subtle)
                        .child(detail),
                );
            }
            let target = crate::ui::profile_panel::activity_target(&agent);
            let item = div()
                .id(SharedString::from(format!("activity-view-{id}")))
                .flex()
                .items_center()
                .gap(px(8.))
                .flex_1()
                .min_w_0()
                .p(px(6.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|style| style.bg(c.affordance_subtle_hover))
                .tooltip(buzz_ui::Tooltip::text(format!("View activity for {name}")))
                .child(
                    avatar(&name, &agent, &directory.profiles, &directory.media)
                        .size(AvatarSize::Small),
                )
                .child(text)
                .when_some(target.filter(|_| !self.closed), |d, target| {
                    d.on_click(move |_, window, cx| {
                        crate::ui::navigation::open_url(&target, window, cx)
                    })
                });
            let dismiss = error.then(|| {
                let (scope, agent, message) = (scope.clone(), agent.clone(), self.message.to_hex());
                IconButton::new(
                    SharedString::from(format!("activity-dismiss-{id}")),
                    IconName::X,
                )
                .size(IconButtonSize::Xs)
                .tooltip(format!("Dismiss error from {name}"))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cx.default_global::<LocalActivity>()
                        .dismiss_error(&scope, &agent, &message);
                })
            });
            popover = popover.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(item)
                    .children(dismiss),
            );
        }
        popover
    }
}
