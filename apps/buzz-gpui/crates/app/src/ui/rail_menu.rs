//! The community rail (`CommunityRail.tsx`, `CommunityRailItem.tsx`):
//! Personal space, one button per community with its NIP-11 icon or
//! initials, and each item's context menu: mark everything read, copy the
//! community URL, invite people to the selected community (owners and
//! admins only), open the community's settings, and leave it.
//!
//! Communities can be reordered by dragging them on the rail or with Move
//! up / Move down in their menu; buzz-app's rail keeps the order they were
//! added in.

use super::modal::Dismissed;
use super::navigation::{Place, Target};
use super::settings::Section;
use super::workspace::Workspace;
use super::{moderation, notice};
use crate::community::{AppState, Community};
use buzz_relay_client::{SessionSnapshot, SessionStatus};
use buzz_ui::{
    ActiveTheme, AlertDialog, Avatar, AvatarSize, Button, IconButton, IconButtonSize, IconName,
    Menu, MenuItem, MenuLabel, MenuSeparator, MenuSize, Surface, ToastTone,
};
use gpui_kit::*;

/// The open menu: the rail item and where the pointer opened it.
pub type MenuState = Entity<Option<(usize, Point<Pixels>)>>;

pub fn state(window: &mut Window, cx: &mut App) -> MenuState {
    window.use_keyed_state("community-rail-menu", cx, |_, _| None)
}

/// The rail: Personal space, the communities, then "Add a community".
pub fn rail(
    marks: Entity<RailMarks>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let menu = state(window, cx);
    let state = cx.global::<AppState>();
    let selected = state.selected;
    let communities = state.communities.clone();
    let mut rail = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(rems(0.5))
        .w(rems(3.))
        .py(rems(0.5))
        .child(
            IconButton::new("personal-space", IconName::Globe)
                .size(IconButtonSize::Md)
                .tooltip("Personal space")
                .selected(selected.is_none())
                .on_click(cx.listener(|this, _, window, cx| this.select_personal(window, cx))),
        );
    let c = *cx.colors();
    for (index, community) in communities.iter().enumerate() {
        let (name, icon, media, (unread, attention)) = {
            let community = community.read(cx);
            (
                community.name(),
                community.icon().map(str::to_owned),
                community.session.media().clone(),
                // You are in the selected one; its sidebar shows the rest.
                if selected == Some(index) {
                    (false, false)
                } else {
                    marks.read(cx).of(&communities[index])
                },
            )
        };
        let image = icon.and_then(|icon| icon_source(&icon, &media));
        let mut avatar = Avatar::new(name.clone()).squircle().size(AvatarSize::Small);
        if let Some(image) = image.clone() {
            avatar = avatar.image(image);
        }
        let menu = menu.clone();
        let dragged = DraggedCommunity {
            index,
            name: name.clone().into(),
            image,
        };
        // Unread, as on a channel row: a pill half out of the rail's left
        // edge, level with the item. One mark for both: unread outside
        // muted channels, or anything needing attention, even from a
        // muted one.
        let pill = (unread || attention).then(|| {
            div()
                .absolute()
                .left(px(-4.))
                .top_0()
                .bottom_0()
                .flex()
                .items_center()
                .child(div().size(px(8.)).rounded_full().bg(c.text_standard))
        });
        let item = div()
            .id(SharedString::from(format!("rail-item-{index}")))
            .relative()
            .rounded(rems(0.5))
            .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
            .drag_over::<DraggedCommunity>(|style, _, _, cx| {
                style.bg(cx.colors().affordance_accent)
            })
            .on_drop(cx.listener(move |_, dragged: &DraggedCommunity, _, cx| {
                AppState::move_community(dragged.index, index, cx);
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, move |event, _, cx| {
                open(&menu, index, event.position, cx)
            })
            .child(
                IconButton::new(
                    SharedString::from(format!("community-{index}")),
                    IconName::Globe,
                )
                .size(IconButtonSize::Md)
                .tooltip(name)
                .selected(selected == Some(index))
                .child(avatar)
                .on_click(cx.listener(move |this, _, window, cx| {
                    crate::perf::begin(crate::perf::SWITCH);
                    this.select_community(index, window, cx);
                    crate::perf::step(crate::perf::SWITCH, "handled");
                })),
            );
        rail = rail.child(
            div()
                .relative()
                .w_full()
                .flex()
                .justify_center()
                .children(pill)
                .child(item),
        );
    }
    rail.children(render(&menu, cx)).child(
        IconButton::new("add-community", IconName::Plus)
            .size(IconButtonSize::Md)
            .tooltip("Add a community")
            .on_click(cx.listener(|this, _, window, cx| this.open_add_community(window, cx))),
    )
}

/// Each community's unread marks, kept as their sessions change: every
/// community, selected or not, so the rail shows where there is something
/// new. It notifies only when a mark changes, so a busy background
/// community does not redraw the window.
pub struct RailMarks {
    marks: std::collections::HashMap<EntityId, (bool, bool)>,
    _observers: Vec<Subscription>,
}

impl RailMarks {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.observe_global::<AppState>(|this, cx| this.follow(cx))
            .detach();
        let mut this = Self {
            marks: Default::default(),
            _observers: Vec::new(),
        };
        this.follow(cx);
        this
    }

    /// Watches the communities saved now.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let communities = cx.global::<AppState>().communities.clone();
        self._observers = communities
            .iter()
            .map(|community| cx.observe(community, |this, community, cx| this.mark(&community, cx)))
            .collect();
        self.marks
            .retain(|id, _| communities.iter().any(|c| c.entity_id() == *id));
        for community in &communities {
            self.mark(community, cx);
        }
    }

    fn mark(&mut self, community: &Entity<Community>, cx: &mut Context<Self>) {
        let marks = unread_marks(&community.read(cx).snapshot);
        if self.marks.insert(community.entity_id(), marks) != Some(marks) {
            cx.notify();
        }
    }

    fn of(&self, community: &Entity<Community>) -> (bool, bool) {
        self.marks
            .get(&community.entity_id())
            .copied()
            .unwrap_or_default()
    }
}

/// Whether a community has unread messages outside muted channels, and
/// whether any unread needs attention (a mention, direct message, followed
/// thread or broadcast), which shows even from a muted channel. Only rows
/// the sidebar lists count.
fn unread_marks(snapshot: &SessionSnapshot) -> (bool, bool) {
    let muted: &[String] = snapshot
        .sidebar
        .data
        .as_ref()
        .map_or(&[], |data| data.muted.as_slice());
    let mut marks = (false, false);
    for (channel, unread) in super::sidebar::listed_unread(snapshot) {
        if unread.unread {
            marks.0 |= !muted.contains(&channel.id);
            marks.1 |= unread.priority;
        }
    }
    marks
}

/// A community being dragged on the rail, drawn as its icon under the
/// pointer.
#[derive(Clone)]
struct DraggedCommunity {
    index: usize,
    name: SharedString,
    image: Option<ImageSource>,
}

impl Render for DraggedCommunity {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut avatar = Avatar::new(self.name.clone())
            .squircle()
            .size(AvatarSize::Small);
        if let Some(image) = self.image.clone() {
            avatar = avatar.image(image);
        }
        div().opacity(0.85).child(avatar)
    }
}

/// A community icon: inline icons decode here, HTTPS icons load like any
/// remote image.
fn icon_source(icon: &str, media: &buzz_relay_client::media::Media) -> Option<ImageSource> {
    if !icon.starts_with("data:") {
        return Some(super::media::image(icon, media));
    }
    super::media::inline_image(icon)
}

/// Opens the menu for rail item `index`. For the selected community the
/// roster is read again: registration is not authorization, so "Invite to
/// community" follows the viewer's current role.
pub fn open(state: &MenuState, index: usize, at: Point<Pixels>, cx: &mut App) {
    if cx.global::<AppState>().selected == Some(index) {
        moderation::refresh_role(cx);
    }
    state.update(cx, |open, cx| {
        *open = Some((index, at));
        cx.notify();
    });
}

/// Why "Mark all as read" is unavailable, if it is (`MarkAllReadItem`):
/// read state belongs to the community's ready session. Unlike buzz-app,
/// whose only live session is the selected community's, every community
/// here keeps its own, so any of them can be marked from the rail.
fn mark_all_reason(community: &Community) -> Option<String> {
    if community.snapshot.status != SessionStatus::Ready {
        Some(format!("Waiting for {} to connect.", community.name()))
    } else if community.snapshot.cached {
        Some("Read state can’t sync on this connection.".into())
    } else {
        None
    }
}

fn close(state: &MenuState, cx: &mut App) {
    state.update(cx, |open, cx| {
        *open = None;
        cx.notify();
    })
}

pub fn render(state: &MenuState, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    let (index, at) = (*state.read(cx))?;
    let app = cx.global::<AppState>();
    let selected = app.selected == Some(index);
    let community = app.communities.get(index)?.clone();
    let origin = community.read(cx).origin.clone();
    let settings = |section: Section| Target {
        community: Some(origin.to_string()),
        place: Place::Settings(section),
    };
    let reason = mark_all_reason(community.read(cx));
    let mut menu = Menu::new().size(MenuSize::Compact).child(
        MenuItem::new("rail-mark-read", "Mark all as read")
            .icon(IconName::Checks)
            .icon_size(14.)
            .size(MenuSize::Compact)
            .disabled(reason.is_some())
            .on_click({
                let (state, session) = (state.clone(), community.read(cx).session.clone());
                move |_, _, cx| {
                    close(&state, cx);
                    let session = session.clone();
                    cx.spawn(async move |cx| {
                        let result = session.mark_all_read().await;
                        cx.update(|cx| match result {
                            Ok(_) => notice::success("Marked all as read.", cx),
                            Err(_) => notice::error("Couldn’t mark everything as read.", cx),
                        });
                    })
                    .detach();
                }
            }),
    );
    if let Some(reason) = reason {
        menu = menu.child(MenuLabel::new(reason).quiet());
    }
    let url = origin.http();
    menu = menu.child(MenuSeparator::new()).child(
        MenuItem::new("rail-copy-url", "Copy community URL")
            .icon(IconName::Link)
            .icon_size(14.)
            .size(MenuSize::Compact)
            .on_click({
                let state = state.clone();
                move |_, _, cx| {
                    close(&state, cx);
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    notice::success("Community URL copied.", cx);
                }
            }),
    );
    if selected && moderation::manager(cx) {
        let (state, target) = (
            state.clone(),
            settings(Section::Card(moderation::MEMBERSHIP)),
        );
        menu = menu.child(
            MenuItem::new("rail-invite", "Invite to community")
                .icon(IconName::Ticket)
                .icon_size(14.)
                .size(MenuSize::Compact)
                .on_click(cx.listener(move |this, _, window, cx| {
                    close(&state, cx);
                    this.navigate(target.clone(), window, cx);
                })),
        );
    }
    let count = app.communities.len();
    if count > 1 {
        let (up, down) = (state.clone(), state.clone());
        menu = menu
            .child(MenuSeparator::new())
            .child(
                MenuItem::new("rail-move-up", "Move up")
                    .icon(IconName::ArrowUp)
                    .icon_size(14.)
                    .size(MenuSize::Compact)
                    .disabled(index == 0)
                    .on_click(move |_, _, cx| {
                        close(&up, cx);
                        AppState::move_community(index, index - 1, cx);
                    }),
            )
            .child(
                MenuItem::new("rail-move-down", "Move down")
                    .icon(IconName::ArrowDown)
                    .icon_size(14.)
                    .size(MenuSize::Compact)
                    .disabled(index + 1 >= count)
                    .on_click(move |_, _, cx| {
                        close(&down, cx);
                        AppState::move_community(index, index + 1, cx);
                    }),
            );
    }
    let (close_state, target) = (state.clone(), settings(Section::Profile));
    menu = menu.child(
        MenuItem::new("rail-settings", "Community settings")
            .icon(IconName::Gear)
            .icon_size(14.)
            .size(MenuSize::Compact)
            .on_click(cx.listener(move |this, _, window, cx| {
                close(&close_state, cx);
                this.navigate(target.clone(), window, cx);
            })),
    );
    let leave_state = state.clone();
    menu = menu.child(MenuSeparator::new()).child(
        MenuItem::new("rail-leave", "Leave community")
            .icon(IconName::SignOut)
            .icon_size(14.)
            .size(MenuSize::Compact)
            .danger()
            .on_click(cx.listener(move |this, _, _, cx| {
                close(&leave_state, cx);
                let community = community.clone();
                let dialog = cx.new(|_| LeaveCommunity {
                    community,
                    busy: false,
                });
                this.show_dialog(dialog, cx);
            })),
    );
    let outside = state.clone();
    Some(
        deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Window)
                .position(at)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .occlude()
                        .on_mouse_down_out(move |_, _, cx| close(&outside, cx))
                        .child(menu),
                ),
        )
        .with_priority(2)
        .into_any_element(),
    )
}

/// "Leave <name>?" from the rail. The rail toasts any failure and closes,
/// as buzz-app's rail does; Settings → Communities keeps its own dialog.
struct LeaveCommunity {
    community: Entity<Community>,
    busy: bool,
}

impl EventEmitter<Dismissed> for LeaveCommunity {}

impl LeaveCommunity {
    fn leave(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let name = self.community.read(cx).name();
        let task = AppState::leave(self.community.clone(), cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |_, cx| {
                match result {
                    Ok((outcome, residue)) => {
                        let (text, tone) = super::settings::left_text(&name, outcome, residue);
                        let timeout = (tone == ToastTone::Success)
                            .then_some(std::time::Duration::from_secs(4));
                        notice::show(text, tone, timeout, cx);
                    }
                    Err(error) => notice::error(leave_failure(&name, &error), cx),
                }
                cx.emit(Dismissed);
            });
        })
        .detach();
        cx.notify();
    }
}

/// `leaveFailureText`: the owner's refusal by name, anything else one retry
/// message.
fn leave_failure(name: &str, error: &str) -> String {
    if error == buzz_relay_client::account::OWNER_CANNOT_LEAVE {
        format!("The relay owner can’t leave {name}.")
    } else {
        format!("Couldn’t leave {name}. Check your connection and try again.")
    }
}

impl Render for LeaveCommunity {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.community.read(cx).name();
        AlertDialog::new(format!("Leave {name}?"))
            .description(format!(
                "This sends a leave request to the community’s relay, then removes {name} \
                 from this device along with its saved drafts and reading positions. \
                 Rejoining may need a new invite."
            ))
            .action(
                Button::new("rail-leave-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .disabled(self.busy)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismissed))),
            )
            .action(
                Button::new("rail-leave-confirm", "Leave community")
                    .destructive()
                    .loading(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.leave(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn rail_marks_count_listed_rows_and_mute_spares_attention() {
        use super::super::sidebar::unread_snapshot;
        use buzz_relay_client::sidebar::SidebarPreferences;
        use buzz_relay_client::sidebar_store::SidebarState;
        use buzz_relay_client::{ChannelSummary, ChannelType, ChannelUnread};
        let channel = |id: &str, kind: ChannelType| ChannelSummary {
            id: id.into(),
            channel_type: Some(kind),
            ..Default::default()
        };
        let ordinary = ChannelUnread {
            unread: true,
            bold: true,
            count: 1,
            ..Default::default()
        };
        let attention = ChannelUnread {
            unread: true,
            priority: true,
            count: 1,
            attention: 1,
            ..Default::default()
        };
        let muted = |ids: &[&str]| SidebarState {
            data: Some(SidebarPreferences {
                muted: ids.iter().map(|id| id.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let general = channel("general", ChannelType::Stream);
        let dm = channel("dm", ChannelType::Dm);
        let marks = |channels: Vec<ChannelSummary>, rows: &[(&str, ChannelUnread)], sidebar| {
            unread_marks(&unread_snapshot(channels, rows, sidebar))
        };
        assert_eq!(
            marks(vec![general.clone()], &[], muted(&[])),
            (false, false)
        );
        assert_eq!(
            marks(vec![general.clone()], &[("general", ordinary)], muted(&[])),
            (true, false)
        );
        // Muting quiets the pill but not a mention.
        assert_eq!(
            marks(
                vec![general.clone()],
                &[("general", ordinary)],
                muted(&["general"])
            ),
            (false, false)
        );
        assert_eq!(
            marks(
                vec![general.clone()],
                &[("general", attention)],
                muted(&["general"])
            ),
            (false, true)
        );
        // Rows the sidebar does not list mark nothing: archived and hidden
        // channels, DMs hidden here or by the relay, and channels no
        // longer in the roster.
        let archived = ChannelSummary {
            archived: true,
            ..channel("archived", ChannelType::Stream)
        };
        let secret = ChannelSummary {
            hidden: true,
            ..channel("secret", ChannelType::Stream)
        };
        let rows = [
            ("archived", attention),
            ("secret", attention),
            ("dm", attention),
            ("gone", attention),
        ];
        let local = SidebarState {
            hidden_dms: vec!["dm".into()],
            ..Default::default()
        };
        assert_eq!(
            marks(vec![archived, secret, dm.clone()], &rows, local),
            (false, false)
        );
        let mut snapshot = unread_snapshot(vec![dm.clone()], &rows, SidebarState::default());
        assert_eq!(unread_marks(&snapshot), (true, true));
        snapshot.hidden_dms = vec!["dm".to_string()].into();
        assert_eq!(unread_marks(&snapshot), (false, false));
    }

    #[test]
    fn leave_failures_follow_the_reference() {
        assert_eq!(
            leave_failure("Hive", buzz_relay_client::account::OWNER_CANNOT_LEAVE),
            "The relay owner can’t leave Hive."
        );
        assert_eq!(
            leave_failure("Hive", "timed out"),
            "Couldn’t leave Hive. Check your connection and try again."
        );
    }
}
