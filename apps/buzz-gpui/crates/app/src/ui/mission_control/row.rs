//! One agent's row: who and where it is, its status and what needs its
//! owner, what it is doing now and its recent usage, and its menu.
//! Each row is its own view, so a tick that changes one agent's elapsed
//! time redraws that row and reuses the others' last frame.

use crate::community::AppState;
use crate::mission_control::model::{Entry, Link, Need, Status, status_label};
use crate::ui::navigation::{self, Target};
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Badge, BadgeTone, Colors, Icon, IconButton, IconButtonSize,
    IconName, Menu, MenuItem, MenuSeparator, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;
use std::time::Duration;

/// Every row is this tall.
pub const HEIGHT: Rems = rems(4.25);

pub struct AgentRow {
    pub entry: Entry,
    menu: bool,
}

impl AgentRow {
    pub fn new(entry: Entry) -> Self {
        Self { entry, menu: false }
    }

    /// An open menu draws every frame, as overlays expect.
    pub fn overlay_open(&self) -> bool {
        self.menu
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = false;
        cx.notify();
    }

    fn controls(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(rems(0.25))
            .child(self.menu(window, cx))
    }

    fn menu(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let entry = &self.entry;
        let id = SharedString::from(format!("mc-menu-{}-{}", entry.key.host, entry.key.id));
        let open = self.menu;
        let anchor = div().relative().child(
            IconButton::new(id, IconName::DotsThree)
                .size(IconButtonSize::Sm)
                .selected(open)
                .when(!open, |this| {
                    this.tooltip(format!("Actions for {}", entry.name))
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.menu = !this.menu;
                    cx.notify();
                })),
        );
        if !open {
            return anchor;
        }
        let pubkey = entry.pubkey.clone();
        let item = |id: &'static str, label: &'static str, icon: IconName| {
            MenuItem::new(id, label).icon(icon).icon_size(14.)
        };
        let mut menu = Menu::new();
        if let Some((origin, channel)) = entry.place.clone() {
            let (to, at) = (origin.clone(), channel.clone());
            menu = menu.child(item("mc-channel", "Open channel", IconName::Hash).on_click(
                cx.listener(move |this, _, window, cx| {
                    this.close_menu(cx);
                    open_channel(to.clone(), at.clone(), window, cx);
                }),
            ));
            let agent = pubkey.clone();
            menu = menu.child(
                item("mc-activity", "View activity", IconName::ListBullets).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.close_menu(cx);
                        open_activity(origin.clone(), channel.clone(), agent.clone(), window, cx);
                    },
                )),
            );
        }
        let agent = pubkey.clone();
        menu = menu
            .child(
                item("mc-message", "Message", IconName::ChatCircle)
                    .disabled(pubkey.is_empty())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_menu(cx);
                        message(agent.clone(), window, cx);
                    })),
            )
            .child(MenuSeparator::new());
        let agent = pubkey.clone();
        menu = menu.child(
            item("mc-open-agent", "Open agent", IconName::Robot)
                .disabled(pubkey.is_empty())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_menu(cx);
                    open_agent(&agent, cx);
                })),
        );
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.close_menu(cx));
        });
        anchor.child(overlay::dropdown(window, Align::End, menu, dismiss))
    }
}

/// The status pill: a dot in the status's tone and its name. A turn in
/// progress reads "Working" here rather than animating the avatar, so a
/// busy fleet redraws once a second, for its timers, not every frame.
fn status_pill(status: Status, active: bool, link: Link, c: &Colors) -> Div {
    let (dot, text) = match status {
        _ if link != Link::Connected => (c.text_unavailable, c.text_metadata),
        _ if active => (c.border_accent, c.text_accent),
        Status::Working | Status::Ready => (c.status_online, c.text_standard),
        Status::Connecting => (c.border_warning, c.text_subtle),
        Status::Elsewhere => (c.text_unavailable, c.text_subtle),
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(rems(0.375))
        .text_caption()
        .text_color(text)
        .child(div().size(rems(0.5)).rounded_full().bg(dot))
        .child(if active && link == Link::Connected {
            "Working"
        } else {
            status_label(status)
        })
}

impl Render for AgentRow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let entry = self.entry.clone();
        let live = entry.link == Link::Connected;
        let tone = entry.attention.as_ref().map(|a| match a.need {
            Need::Failed => (c.border_danger, c.text_danger, BadgeTone::Danger),
            Need::Waiting | Need::Stuck => (c.border_warning, c.text_warning, BadgeTone::Warning),
        });
        let mut avatar = Avatar::new(entry.name.clone())
            .squircle()
            .size(AvatarSize::Px(36.))
            .alt(entry.name.clone());
        if let Some(picture) = picture(&entry, cx) {
            avatar = avatar.image(picture);
        }
        let who = div()
            .flex()
            .flex_col()
            .w(rems(15.))
            .flex_none()
            .min_w_0()
            .gap(rems(0.125))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .min_w_0()
                    .child(div().text_label_sm().truncate().child(entry.name.clone()))
                    .children(
                        entry
                            .attention
                            .as_ref()
                            .zip(tone)
                            .map(|(a, (_, _, badge))| Badge::new(a.need.label()).tone(badge)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.25))
                    .min_w_0()
                    .text_caption()
                    .text_color(c.text_metadata)
                    .child(
                        Icon::new(if entry.remote {
                            IconName::Globe
                        } else {
                            IconName::Monitor
                        })
                        .size(12.)
                        .color(c.text_metadata),
                    )
                    .child(div().truncate().child(match &entry.model {
                        Some(model) => format!("{} · {model}", entry.host_label),
                        None => entry.host_label.clone(),
                    })),
            );
        let reason = entry
            .attention
            .as_ref()
            .map(|a| a.reason.clone())
            .filter(|reason| *reason != entry.doing.text);
        let now = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(rems(0.125))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .min_w_0()
                    .text_body_sm()
                    .text_color(if entry.doing.active {
                        c.text_standard
                    } else {
                        c.text_subtle
                    })
                    .child(div().truncate().child(entry.doing.text.clone()))
                    .children(entry.doing.elapsed.clone().map(|elapsed| {
                        div()
                            .flex_none()
                            .text_caption()
                            .text_color(c.text_metadata)
                            .child(elapsed)
                    })),
            )
            .children(reason.zip(tone).map(|(reason, (_, text, _))| {
                div()
                    .text_caption()
                    .text_color(text)
                    .truncate()
                    .child(reason)
            }));
        let usage = div()
            .flex()
            .flex_col()
            .items_end()
            .w(rems(11.))
            .flex_none()
            .text_caption()
            .children(
                entry
                    .usage
                    .clone()
                    .map(|usage| div().text_color(c.text_subtle).truncate().child(usage)),
            )
            .children(
                entry
                    .usage
                    .is_some()
                    .then(|| div().text_color(c.text_metadata).child("last 24 hours")),
            );
        div()
            .id(SharedString::from(format!(
                "mc-row-{}-{}",
                entry.key.host, entry.key.id
            )))
            .relative()
            .flex()
            .items_center()
            .gap(rems(1.))
            .w_full()
            .h(HEIGHT)
            .pl(rems(1.))
            .pr(rems(0.75))
            .hover(|this| this.bg(c.affordance_panel_hover))
            .when(!live, |this| this.opacity(0.55))
            .when_some(tone, |this, (bar, _, _)| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(rems(0.75))
                        .bottom(rems(0.75))
                        .w(px(3.))
                        .rounded_r(px(2.))
                        .bg(bar),
                )
            })
            .child(div().flex_none().child(avatar))
            .child(who)
            .child(div().w(rems(5.5)).flex_none().child(status_pill(
                entry.status,
                entry.doing.active,
                entry.link,
                &c,
            )))
            .child(now)
            .child(usage)
            .child(self.controls(window, cx))
    }
}

/// The saved picture, else the profile the selected community knows.
fn picture(entry: &Entry, cx: &App) -> Option<ImageSource> {
    let url = entry.picture.clone().or_else(|| {
        AppState::current(cx)?
            .read(cx)
            .snapshot
            .profiles
            .get(&entry.pubkey)
            .and_then(|profile| profile.picture.clone())
    })?;
    // Fetched through the community the picture belongs to, which signs
    // for its media.
    Some(crate::ui::media::any_avatar(&url))
}

/// Opens the agent's Build view on the Agents page, through its route.
fn open_agent(pubkey: &str, cx: &mut App) {
    let target = crate::ui::agents2::agent_target(pubkey, cx);
    navigation::with_workspace(cx, move |workspace, window, cx| {
        workspace.navigate(target, window, cx)
    });
}

fn open_channel(origin: String, channel: String, window: &mut Window, cx: &mut App) {
    let _ = window;
    navigation::with_workspace(cx, move |workspace, window, cx| {
        workspace.navigate(Target::channel(Some(origin), channel), window, cx)
    });
}

/// How long the activity waits for the agent's channel to show.
const ACTIVITY_WAIT: Duration = Duration::from_secs(10);

/// Opens the channel the agent last worked in, then its activity beside
/// it, once the conversation shows.
fn open_activity(
    origin: String,
    channel: String,
    agent: String,
    window: &mut Window,
    cx: &mut App,
) {
    let _ = window;
    let url = crate::agent_activity::target(&agent, Some(&channel), None);
    navigation::with_workspace(cx, move |workspace, window, cx| {
        workspace.navigate(Target::channel(Some(origin), channel), window, cx);
        // Navigating closes the old conversation, so a panel link opens
        // only once the new one shows, which can take a fetch.
        if workspace.open_panel_link(&url, window, cx) {
            return;
        }
        let opened = Rc::new(std::cell::Cell::new(false));
        let watch = cx.observe_in(&cx.entity(), window, {
            let opened = opened.clone();
            move |workspace, _, window, cx| {
                if !opened.get() && workspace.open_panel_link(&url, window, cx) {
                    opened.set(true);
                }
            }
        });
        // Gives up when the channel never shows.
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(ACTIVITY_WAIT).await;
            drop(watch);
        })
        .detach();
    });
}

/// A direct message with the agent in the selected community.
fn message(agent: String, window: &mut Window, cx: &mut App) {
    let Some(community) = AppState::current(cx) else {
        return;
    };
    let session = community.read(cx).session.clone();
    window
        .spawn(cx, async move |cx| {
            match session.open_dm(vec![agent]).await {
                Ok(id) => {
                    let _ = cx.update(|window, cx| navigation::open_conversation(id, window, cx));
                }
                Err(error) => {
                    let _ = cx.update(|_, cx| {
                        crate::ui::notice::error(format!("Couldn’t open a message. {error}"), cx)
                    });
                }
            }
        })
        .detach();
}
