//! Me's sidebar (`me/MeSidebar.tsx`) and its grouped history
//! (`sessions/SessionSections.tsx`): "New chat", the viewer's Me groups
//! as Projects, then "Chats", each row with its actions menu. Rows share
//! the Messages sidebar's working indicator and activity popover.
//!
//! Groups and moves go through the Me groups store, which shows a move at
//! once and keeps a failure until it is retried or dismissed.

use super::{MeModel, Selection, dialogs};
use crate::community::{AppState, Community};
use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::sidebar::menus::channel_link;
use crate::ui::toasts::{self, Notice};
use buzz_relay_client::sidebar::NewSection;
use buzz_relay_client::sidebar_store::PreferencesStatus;
use buzz_relay_client::{ChannelSummary, RosterStatus, SessionSnapshot, SessionStatus};
use buzz_ui::{
    ActiveTheme, Button, Icon, IconButton, IconButtonSize, IconName, Menu, MenuItem, Surface,
    ToastTone, TypeStyled, tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// Collapsed groups, saved per community (`me:collapsed-sections`).
const COLLAPSED: &str = "me:collapsed-sections";

/// A row's open actions menu: from its button, or at the pointer.
struct RowMenu {
    id: String,
    title: String,
    /// The group the row is in; empty for Conversations.
    section: String,
    at: Option<Point<Pixels>>,
    sub: Sub,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sub {
    None,
    Move,
    Copy,
}

/// One group of rows: a Me group, or Conversations (empty id).
struct Group<'a> {
    id: String,
    name: String,
    rows: Vec<&'a ChannelSummary>,
}

/// Where the list showed at the last paint: its visible bounds and its
/// scroll offset.
type Painted = (Bounds<Pixels>, Pixels);

/// What every row of a render shares, read once for all of them.
struct Shared<'a> {
    community: &'a Entity<Community>,
    snapshot: &'a SessionSnapshot,
    own: &'a Arc<HashSet<String>>,
    selected: Option<&'a str>,
}

pub struct MeSidebar {
    model: Entity<MeModel>,
    collapsed: Vec<String>,
    /// The community `collapsed`, the menus and the delete state are for.
    collapsed_for: Option<EntityId>,
    menu: Option<RowMenu>,
    /// The group whose header menu is open.
    section_menu: Option<String>,
    dialog: Option<dialogs::Dialog>,
    deleting: Option<String>,
    delete_error: Option<SharedString>,
    scroll: ScrollHandle,
    /// Row bounds as they are painted.
    bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// Row bounds from the last paint, taken at the start of a render to
    /// pick the rows drawn in full ([`Self::row_shown`]).
    painted: HashMap<String, Bounds<Pixels>>,
    /// The list's visible bounds and scroll offset at the last paint.
    viewport: Rc<Cell<Option<Painted>>>,
    _model: Subscription,
}

impl MeSidebar {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = MeModel::global(cx);
        let subscription = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            collapsed: Vec::new(),
            collapsed_for: None,
            menu: None,
            section_menu: None,
            dialog: None,
            deleting: None,
            delete_error: None,
            scroll: ScrollHandle::new(),
            bounds: Rc::default(),
            painted: HashMap::new(),
            viewport: Rc::default(),
            _model: subscription,
        }
    }

    fn community(&self, cx: &App) -> Option<Entity<Community>> {
        self.model.read(cx).community.clone()
    }

    /// Reads the collapsed groups when the community changes, and drops
    /// the menus and delete state of the one before.
    fn load_collapsed(&mut self, community: &Entity<Community>, cx: &App) {
        if self.collapsed_for == Some(community.entity_id()) {
            return;
        }
        self.collapsed_for = Some(community.entity_id());
        self.menu = None;
        self.section_menu = None;
        self.deleting = None;
        self.delete_error = None;
        self.collapsed = cx
            .global::<AppState>()
            .store
            .view_state(&community.read(cx).partition(), COLLAPSED)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
    }

    fn toggle(&mut self, id: &str, open: bool, cx: &mut Context<Self>) {
        self.collapsed.retain(|key| key != id);
        if !open {
            self.collapsed.push(id.to_owned());
        }
        if let Some(community) = self.community(cx) {
            let json = serde_json::to_string(&self.collapsed).expect("ids serialize");
            let _ = cx.global::<AppState>().store.set_view_state(
                &community.read(cx).partition(),
                COLLAPSED,
                Some(&json),
            );
        }
        cx.notify();
    }

    fn select(&self, selection: Selection, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.select(selection, cx));
    }

    fn close_menus(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        self.section_menu = None;
        cx.notify();
    }

    /// Moves a conversation to a group (`None`: Conversations).
    fn move_to(&mut self, id: &str, section: Option<String>, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(community) = self.community(cx) else {
            return;
        };
        self.toggle(section.as_deref().unwrap_or(""), true, cx);
        let kit = community.read(cx).session.me_kit().clone();
        let id = id.to_owned();
        cx.background_spawn(async move {
            let _ = kit.me_assign(&id, section).await;
        })
        .detach();
    }

    fn copy(&mut self, value: String, label: &str, cx: &mut Context<Self>) {
        self.close_menus(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        // GPUI's clipboard write cannot fail, so there is no error notice.
        toasts::show(
            Notice {
                key: "me-copy".into(),
                title: format!("{label} copied").into(),
                description: None,
                tone: ToastTone::Success,
                timeout: Some(Duration::from_secs(4)),
                dismissible: true,
                actions: Vec::new(),
            },
            cx,
        );
    }

    fn delete_section(&mut self, id: String, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(community) = self.community(cx) else {
            return;
        };
        self.deleting = Some(id.clone());
        self.delete_error = None;
        let owner = Some(community.entity_id());
        let kit = community.read(cx).session.me_kit().clone();
        cx.spawn(async move |this, cx| {
            let result = kit.me_remove_section(&id).await;
            let _ = this.update(cx, |this, cx| {
                if this.collapsed_for != owner {
                    return;
                }
                this.deleting = None;
                if let Err(error) = result {
                    this.delete_error = Some(if error.is_empty() {
                        "Section could not be deleted. Try again.".into()
                    } else {
                        error.into()
                    });
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_dialog(&mut self, dialog: dialogs::Dialog, cx: &mut Context<Self>) {
        self.close_menus(cx);
        self.dialog = Some(dialog);
        cx.notify();
    }

    /// The groups and their rows (`sections` in `SessionSections`).
    fn groups<'a>(&self, conversations: &'a [ChannelSummary], cx: &App) -> Vec<Group<'a>> {
        let me = &self.model.read(cx).me;
        let data = me.data.as_ref();
        let sections = data.map(|d| d.sections.as_slice()).unwrap_or_default();
        let assigned = |id: &str| data.and_then(|d| d.assignments.get(id));
        let mut groups: Vec<Group> = sections
            .iter()
            .map(|section| Group {
                id: section.id.clone(),
                name: section.name.clone(),
                rows: conversations
                    .iter()
                    .filter(|c| assigned(&c.id) == Some(&section.id))
                    .collect(),
            })
            .collect();
        let ids: HashSet<&String> = sections.iter().map(|s| &s.id).collect();
        groups.push(Group {
            id: String::new(),
            name: "Chats".into(),
            rows: conversations
                .iter()
                .filter(|c| assigned(&c.id).is_none_or(|section| !ids.contains(section)))
                .collect(),
        });
        // Me hides an empty Chats group.
        groups.retain(|group| !group.id.is_empty() || !group.rows.is_empty());
        groups
    }

    fn heading_icon(icon: IconName) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w(rems(1.25))
            .h(rems(1.5))
            .child(Icon::new(icon).size(15.).stroke_width(2.5))
    }

    fn new_conversation(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let ix = c.interaction(Surface::Navigation);
        div()
            .id("me-new-conversation")
            .my(rems(0.5))
            .flex()
            .items_center()
            .gap(rems(0.5))
            .min_h(rems(2.))
            .px(rems(0.5))
            .py(px(2.))
            .rounded(radius::ROW)
            .cursor_pointer()
            .text_body_sm()
            .text_color(c.text_standard)
            .hover(move |s| s.bg(ix.row_hover))
            .child(Self::heading_icon(IconName::Plus))
            .child("New chat")
            .on_click(cx.listener(|this, _, _, cx| this.select(Selection::New, cx)))
            .into_any_element()
    }

    fn group(
        &mut self,
        group: &Group,
        shared: &Shared,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let ix = c.interaction(Surface::Navigation);
        let open = !self.collapsed.contains(&group.id);
        let key = group.id.clone();
        let hover: SharedString = format!("me-section-{}", group.id).into();
        let menu_open = self.section_menu.as_deref() == Some(group.id.as_str());
        let caret = Icon::new(if open {
            IconName::CaretDown
        } else {
            IconName::CaretRight
        })
        .size(15.)
        .stroke_width(2.5);
        let mut heading = div()
            .id(SharedString::from(format!("me-heading-{}", group.id)))
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(rems(0.5))
            .min_h(rems(2.))
            .px(rems(0.5))
            .py(px(2.))
            .cursor_pointer()
            .text_body_sm()
            .text_color(c.text_unavailable)
            .on_click(cx.listener(move |this, _, _, cx| {
                let open = !this.collapsed.contains(&key);
                this.toggle(&key, !open, cx);
            }));
        if group.id.is_empty() {
            heading = heading
                .child(div().min_w_0().truncate().child(group.name.clone()))
                .child(caret);
        } else {
            // A project shows its disclosure caret, not a folder (#1005).
            heading = heading
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .w(rems(1.25))
                        .h(rems(1.5))
                        .child(caret),
                )
                .child(div().min_w_0().truncate().child(group.name.clone()));
        }
        let actions = (!group.id.is_empty()).then(|| {
            let (new_id, menu_id) = (group.id.clone(), group.id.clone());
            let name = group.name.clone();
            let menu = menu_open.then(|| {
                let weak = cx.entity().downgrade();
                let dismiss: Dismiss = Rc::new(move |_, cx| {
                    let _ = weak.update(cx, |this, cx| this.close_menus(cx));
                });
                let content = self.section_menu_content(group, cx);
                overlay::dropdown(window, Align::End, content, dismiss)
            });
            div()
                .flex()
                .flex_none()
                .items_center()
                .when(!menu_open, |this| {
                    this.opacity(0.)
                        .group_hover(hover.clone(), |s| s.opacity(1.))
                })
                .child(
                    IconButton::new(
                        SharedString::from(format!("me-new-in-{}", group.id)),
                        IconName::Plus,
                    )
                    .size(IconButtonSize::Sm)
                    .surface(Surface::Navigation)
                    .icon_size(15.)
                    .stroke_width(2.5)
                    .icon_color(c.text_unavailable)
                    .tooltip(format!("New chat in {name}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select(Selection::NewIn(new_id.clone()), cx)
                    })),
                )
                .child(
                    div()
                        .relative()
                        .child(
                            IconButton::new(
                                SharedString::from(format!("me-section-actions-{}", group.id)),
                                IconName::DotsThree,
                            )
                            .size(IconButtonSize::Sm)
                            .surface(Surface::Navigation)
                            .icon_size(15.)
                            .stroke_width(2.5)
                            .icon_color(c.text_unavailable)
                            .when(!menu_open, |b| {
                                b.tooltip(format!("Actions for {}", group.name))
                            })
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.menu = None;
                                    this.section_menu = Some(menu_id.clone());
                                    cx.notify();
                                },
                            )),
                        )
                        .children(menu),
                )
        });
        let header = div()
            .id(hover.clone())
            .group(hover)
            .flex()
            .items_center()
            .gap(rems(0.25))
            .rounded(radius::ROW)
            .when(!group.id.is_empty(), |this| {
                this.hover(move |s| s.bg(ix.row_hover))
                    .when(menu_open, |this| this.bg(ix.row_hover))
            })
            .child(heading)
            .children(actions);
        let rows: Vec<AnyElement> = if open {
            let mut rows: Vec<AnyElement> = group
                .rows
                .iter()
                .map(|row| match self.row_shown(&row.id, shared.selected) {
                    true => self.row(row, &group.id, shared, window, cx),
                    false => self.row_placeholder(&row.id),
                })
                // A project's chats are indented under it.
                .map(|row| match group.id.is_empty() {
                    true => row,
                    false => div().ml(rems(1.25)).child(row).into_any_element(),
                })
                .collect();
            if rows.is_empty() && !group.id.is_empty() {
                rows.push(list_message("No chats in this project.", cx).into_any_element());
            }
            rows
        } else {
            Vec::new()
        };
        div()
            .flex()
            .flex_col()
            .my(rems(0.5))
            .child(header)
            .children(rows)
            .into_any_element()
    }

    fn section_menu_content(&self, group: &Group, cx: &mut Context<Self>) -> AnyElement {
        let writable = self.model.read(cx).me.remove_section_writable();
        let (settings_id, settings_name, delete_id) =
            (group.id.clone(), group.name.clone(), group.id.clone());
        Menu::new()
            .child(
                MenuItem::new("me-section-settings", "Project settings…").on_click(cx.listener(
                    move |this, _, window, cx| {
                        let Some(community) = this.community(cx) else {
                            return;
                        };
                        let dialog = dialogs::Dialog::section_settings(
                            community,
                            settings_id.clone(),
                            settings_name.clone(),
                            window,
                            cx,
                        );
                        this.open_dialog(dialog, cx);
                    },
                )),
            )
            .child(
                MenuItem::new("me-delete-section", "Delete project")
                    .danger()
                    .disabled(!writable || self.deleting.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.delete_section(delete_id.clone(), cx)
                    })),
            )
            .into_any_element()
    }

    /// Whether a row is drawn in full. Rows more than a list height away
    /// from the visible part stand in as [`Self::row_placeholder`]s of the
    /// same height, as in the Messages sidebar. The selected row, the one
    /// with its menu open, and rows not painted yet are always drawn.
    fn row_shown(&self, id: &str, selected: Option<&str>) -> bool {
        let (Some(row), Some((viewport, painted_at))) = (self.painted.get(id), self.viewport.get())
        else {
            return true;
        };
        if selected == Some(id) || self.menu.as_ref().is_some_and(|menu| menu.id == id) {
            return true;
        }
        // Where the row is now: scrolling since the paint moved it.
        let top = row.top() + (self.scroll.offset().y - painted_at);
        let margin = viewport.size.height;
        top + row.size.height >= viewport.top() - margin && top <= viewport.bottom() + margin
    }

    /// Records a row's bounds as painted.
    fn bounds_probe(&self, id: &str) -> impl IntoElement + use<> {
        let (bounds, id) = (self.bounds.clone(), id.to_owned());
        canvas(
            move |row, window, _| {
                // A placeholder that comes into view (a jump the scroll
                // offset did not foresee) asks for the next frame, which
                // draws it in full.
                if row.intersects(&window.content_mask().bounds) {
                    window.request_animation_frame();
                }
                bounds.borrow_mut().insert(id, row);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }

    /// A row far outside the view: its height and bounds, without its
    /// content.
    fn row_placeholder(&self, id: &str) -> AnyElement {
        let height: DefiniteLength = self
            .painted
            .get(id)
            .map_or(rems(2.5).into(), |row| row.size.height.into());
        div()
            .id(SharedString::from(format!("me-row-{id}")))
            .relative()
            .flex_none()
            .h(height)
            .mb(px(2.))
            .child(self.bounds_probe(id))
            .into_any_element()
    }

    /// Records the list's visible bounds and scroll offset as painted, for
    /// [`Self::row_shown`].
    fn viewport_probe(&self) -> impl IntoElement + use<> {
        let (viewport, scroll) = (self.viewport.clone(), self.scroll.clone());
        canvas(
            move |bounds, _, _| viewport.set(Some((bounds, scroll.offset().y))),
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }

    fn row(
        &mut self,
        channel: &ChannelSummary,
        section: &str,
        shared: &Shared,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let ix = c.interaction(Surface::Navigation);
        let id = channel.id.clone();
        let is_selected = shared.selected == Some(id.as_str());
        let menu_open = self
            .menu
            .as_ref()
            .is_some_and(|menu| menu.id == id && menu.at.is_none());
        let group: SharedString = format!("me-row-{id}").into();
        let (community, snapshot) = (shared.community, shared.snapshot);
        let bold = !is_selected && snapshot.unread.channel(&id).bold;
        let agents = crate::ui::agent_activity::with_observed(
            crate::ui::sidebar::working_agents(snapshot, &id, shared.own),
            community,
            &id,
            cx,
        );
        let badge = crate::ui::sidebar::working_badge(
            &id,
            &agents,
            &snapshot.profiles,
            &channel.name,
            false,
            cx,
        );
        let open = id.clone();
        let (menu_id, menu_title, menu_section) =
            (id.clone(), channel.name.clone(), section.to_owned());
        let (ctx_id, ctx_title, ctx_section) =
            (id.clone(), channel.name.clone(), section.to_owned());
        let dropdown = menu_open.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.close_menus(cx));
            });
            let content = self.row_menu_content(cx);
            overlay::dropdown(window, Align::End, content, dismiss)
        });
        let row = div()
            .id(group.clone())
            .group(group.clone())
            .relative()
            .flex()
            .items_center()
            .min_w_0()
            .mb(px(2.))
            .rounded(radius::ROW)
            .when(is_selected, |this| this.bg(ix.selected))
            .when(!is_selected, |this| {
                this.hover(move |s| s.bg(ix.row_hover))
                    .when(menu_open, |this| this.bg(ix.row_hover))
            })
            .child(self.bounds_probe(&id))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.section_menu = None;
                    this.menu = Some(RowMenu {
                        id: ctx_id.clone(),
                        title: ctx_title.clone(),
                        section: ctx_section.clone(),
                        at: Some(event.position),
                        sub: Sub::None,
                    });
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id(SharedString::from(format!("me-open-{id}")))
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .min_h(rems(2.5))
                    .pl(px(10.5))
                    .pr(rems(2.25))
                    .cursor_pointer()
                    .text_body()
                    .text_color(if bold {
                        c.affordance_prominent
                    } else {
                        c.text_standard
                    })
                    .when(bold, |this| this.font_weight(FontWeight::MEDIUM))
                    .child(div().min_w_0().truncate().child(channel.name.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select(Selection::Session(open.clone()), cx)
                    })),
            )
            .child(
                div()
                    .absolute()
                    .right(px(4.))
                    .top_0()
                    .bottom_0()
                    .flex()
                    .items_center()
                    .when(!menu_open, |this| {
                        this.opacity(0.)
                            .group_hover(group.clone(), |s| s.opacity(1.))
                    })
                    .child(
                        div()
                            .relative()
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("me-actions-{id}")),
                                    IconName::DotsThree,
                                )
                                .size(IconButtonSize::Sm)
                                .surface(Surface::Navigation)
                                .icon_size(15.)
                                .stroke_width(2.5)
                                .icon_color(c.text_unavailable)
                                .selected(menu_open)
                                .when(!menu_open, |b| {
                                    b.tooltip(format!("Actions for {}", channel.name))
                                })
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.section_menu = None;
                                        this.menu = Some(RowMenu {
                                            id: menu_id.clone(),
                                            title: menu_title.clone(),
                                            section: menu_section.clone(),
                                            at: None,
                                            sub: Sub::None,
                                        });
                                        cx.notify();
                                    },
                                )),
                            )
                            .children(dropdown),
                    ),
            );
        // Working takes the actions button's slot, as in Messages, and
        // gives way to the button on hover.
        let row = match badge {
            Some(badge) => row.child(
                div()
                    .absolute()
                    .right(px(4.))
                    .top_0()
                    .bottom_0()
                    .w(rems(2.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(menu_open, |this| this.opacity(0.))
                    .group_hover(group, |s| s.opacity(0.))
                    .child(badge),
            ),
            None => row,
        };
        let media = community.read(cx).session.media().clone();
        crate::ui::channel_activity::ActivityRow::new(&id, row)
            .activity(
                snapshot.unread.activity.get(&id).cloned(),
                agents.agents,
                shared.own.clone(),
            )
            .directory(
                snapshot.profiles.clone(),
                Some(media),
                Some(community.clone()),
            )
            .opens(self.opens())
            .into_any_element()
    }

    /// The activity popover's entries: messages and threads open in the
    /// channel reader, as other Me links do; a working agent without a
    /// message, and its activity, open in Me's conversation and its panels.
    fn opens(&self) -> crate::ui::channel_activity::Opens {
        let message = |channel: &str, id: &str, window: &mut Window, cx: &mut App| {
            if let Ok(id) = buzz_relay_client::EventId::from_hex(id) {
                let url = buzz_relay_client::message_link(channel, &id);
                crate::ui::navigation::open_url(&url, window, cx);
            }
        };
        let (agent_model, activity_model) = (self.model.downgrade(), self.model.downgrade());
        crate::ui::channel_activity::Opens {
            thread: Rc::new(message),
            agent: Rc::new(move |channel, _, id, window, cx| match id {
                Some(id) => message(channel, id, window, cx),
                None => {
                    let _ = agent_model.update(cx, |model, cx| {
                        model.select(Selection::Session(channel.to_owned()), cx)
                    });
                }
            }),
            activity: Rc::new(move |channel, agent, _, cx| {
                let _ = activity_model.update(cx, |model, cx| {
                    model.update_tab_state(channel, cx, |state| {
                        state.open_panel(crate::ui::channel_tabs::PanelTab {
                            panel: crate::ui::agent_activity::PANEL,
                            channel: channel.to_owned(),
                            target: crate::agent_activity::target(agent, Some(channel), None),
                            root: None,
                            tool: false,
                        });
                    });
                    model.select(Selection::Session(channel.to_owned()), cx)
                });
            }),
        }
    }

    /// The row actions (`SessionActionsMenu` content), with its submenu.
    fn row_menu_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(menu) = &self.menu else {
            return div().into_any_element();
        };
        let model = self.model.read(cx);
        let writable = model.me.writable;
        let community = model.community.clone();
        let channel = community.as_ref().and_then(|community| {
            community
                .read(cx)
                .snapshot
                .channels
                .iter()
                .find(|c| c.id == menu.id)
                .cloned()
        });
        let read_only = channel.as_ref().is_some_and(|c| c.read_only);
        let (id, title) = (menu.id.clone(), menu.title.clone());
        let sub = menu.sub;
        let share = {
            let (community, channel) = (community.clone(), channel.clone());
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                let (Some(community), Some(channel)) = (community.clone(), channel.clone()) else {
                    return;
                };
                let dialog = dialogs::Dialog::share(community, channel, window, cx);
                this.open_dialog(dialog, cx);
            })
        };
        let rename = {
            let (community, id, title) = (community.clone(), id.clone(), title.clone());
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                let Some(community) = community.clone() else {
                    return;
                };
                let dialog =
                    dialogs::Dialog::rename(community, id.clone(), title.clone(), window, cx);
                this.open_dialog(dialog, cx);
            })
        };
        let settings = {
            let (community, id, title) = (community.clone(), id.clone(), title.clone());
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                let Some(community) = community.clone() else {
                    return;
                };
                let dialog = dialogs::Dialog::session_settings(
                    community,
                    id.clone(),
                    title.clone(),
                    window,
                    cx,
                );
                this.open_dialog(dialog, cx);
            })
        };
        let main = Menu::new()
            .child(MenuItem::new("me-share", "Share…").on_click(share))
            .child(
                MenuItem::new("me-rename", "Rename")
                    .icon(IconName::PencilSimple)
                    .icon_size(16.)
                    .disabled(read_only)
                    .on_click(rename),
            )
            .child(
                div()
                    .id("me-move-trigger")
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.hover_sub(*hovered, Sub::Move, cx)
                    }))
                    .child(
                        MenuItem::new("me-move", "Move to")
                            .icon(IconName::FolderSimple)
                            .icon_size(16.)
                            .submenu()
                            .highlighted(sub == Sub::Move)
                            .disabled(!writable)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sub(Sub::Move, cx))),
                    ),
            )
            .child(
                div()
                    .id("me-copy-trigger")
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.hover_sub(*hovered, Sub::Copy, cx)
                    }))
                    .child(
                        MenuItem::new("me-copy", "Copy")
                            .icon(IconName::Copy)
                            .icon_size(16.)
                            .submenu()
                            .highlighted(sub == Sub::Copy)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sub(Sub::Copy, cx))),
                    ),
            )
            .child(
                div()
                    .id("me-settings-trigger")
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.hover_sub(*hovered, Sub::None, cx)
                    }))
                    .child(
                        MenuItem::new("me-session-settings", "Chat settings…")
                            .icon(IconName::Gear)
                            .icon_size(16.)
                            .disabled(read_only)
                            .on_click(settings),
                    ),
            );
        let submenu = match sub {
            Sub::None => None,
            Sub::Move => Some(self.move_menu(menu, cx)),
            Sub::Copy => Some(self.copy_menu(menu, cx)),
        };
        // The submenu opens beside its trigger row, outside the menu's
        // own box so the menu keeps its place.
        let offset = match sub {
            Sub::Copy => px(112.),
            _ => px(76.),
        };
        div()
            .relative()
            .child(main)
            .children(submenu.map(|submenu| {
                div()
                    .absolute()
                    .left_full()
                    .top(offset)
                    .pl(px(4.))
                    .child(submenu)
            }))
            .into_any_element()
    }

    fn hover_sub(&mut self, hovered: bool, which: Sub, cx: &mut Context<Self>) {
        if hovered && let Some(menu) = &mut self.menu {
            menu.sub = which;
            cx.notify();
        }
    }

    fn toggle_sub(&mut self, which: Sub, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.menu {
            menu.sub = if menu.sub == which { Sub::None } else { which };
        }
        cx.notify();
    }

    fn move_menu(&self, menu: &RowMenu, cx: &mut Context<Self>) -> AnyElement {
        let groups = self
            .model
            .read(cx)
            .me
            .data
            .as_ref()
            .map(|data| data.sections.clone())
            .unwrap_or_default();
        let mut list = Menu::new();
        if !menu.section.is_empty() {
            let id = menu.id.clone();
            list = list.child(
                MenuItem::new("me-move-conversations", "Chats")
                    .on_click(cx.listener(move |this, _, _, cx| this.move_to(&id, None, cx))),
            );
        }
        for group in groups {
            let id = menu.id.clone();
            let target = group.id.clone();
            list = list.child(
                MenuItem::new(
                    SharedString::from(format!("me-move-{}", group.id)),
                    group.name.clone(),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.move_to(&id, Some(target.clone()), cx)),
                ),
            );
        }
        let (id, title) = (menu.id.clone(), menu.title.clone());
        list.child(
            MenuItem::new("me-move-new", "New project…").on_click(cx.listener(
                move |this, _, window, cx| {
                    let dialog =
                        dialogs::Dialog::new_section(id.clone(), title.clone(), window, cx);
                    this.open_dialog(dialog, cx);
                },
            )),
        )
        .into_any_element()
    }

    fn copy_menu(&self, menu: &RowMenu, cx: &mut Context<Self>) -> AnyElement {
        let (name, id, link) = (menu.title.clone(), menu.id.clone(), channel_link(&menu.id));
        Menu::new()
            .child(MenuItem::new("me-copy-name", "Copy chat name").on_click(
                cx.listener(move |this, _, _, cx| this.copy(name.clone(), "Chat name", cx)),
            ))
            .child(
                MenuItem::new("me-copy-id", "Copy chat ID").on_click(
                    cx.listener(move |this, _, _, cx| this.copy(id.clone(), "Chat ID", cx)),
                ),
            )
            .child(
                MenuItem::new("me-copy-link", "Copy link to session").on_click(
                    cx.listener(move |this, _, _, cx| this.copy(link.clone(), "Session link", cx)),
                ),
            )
            .into_any_element()
    }

    /// The row menu opened at the pointer (right-click).
    fn context_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let at = self.menu.as_ref()?.at?;
        let content = self.row_menu_content(cx);
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.close_menus(cx));
        });
        Some(overlay::context_menu(window, at, content, dismiss).into_any_element())
    }

    /// Saving and failed moves (`moves` in `SessionSections`).
    fn moves(&self, conversations: &[ChannelSummary], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let me = self.model.read(cx).me.clone();
        let moves: Vec<_> = me
            .moves
            .iter()
            .filter(|m| conversations.iter().any(|c| c.id == m.intent.channel_id))
            .cloned()
            .collect();
        let mut out = Vec::new();
        if moves.iter().any(|m| m.pending) {
            out.push(list_message("Saving project…", cx).into_any_element());
        }
        for failed in moves.iter().filter(|m| m.error.is_some()) {
            let (retry, dismiss) = (
                failed.intent.channel_id.clone(),
                failed.intent.channel_id.clone(),
            );
            let community = self.community(cx);
            let kit = community.map(|c| c.read(cx).session.me_kit().clone());
            let dismiss_kit = kit.clone();
            out.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .mx(rems(0.75))
                    .my(rems(0.25))
                    .child(
                        div()
                            .text_caption()
                            .text_color(cx.colors().text_danger)
                            .child(failed.error.clone().unwrap_or_default()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(rems(0.5))
                            .child(
                                Button::new(
                                    SharedString::from(format!("me-retry-move-{retry}")),
                                    "Retry move",
                                )
                                .disabled(!me.writable)
                                .on_click(move |_, _, cx| {
                                    if let Some(kit) = kit.clone() {
                                        let id = retry.clone();
                                        cx.background_spawn(async move {
                                            let _ = kit.me_retry_move(&id).await;
                                        })
                                        .detach();
                                    }
                                }),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("me-dismiss-move-{dismiss}")),
                                    "Dismiss",
                                )
                                .on_click(move |_, _, _| {
                                    if let Some(kit) = &dismiss_kit {
                                        kit.me_dismiss_move(&dismiss);
                                    }
                                }),
                            ),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn dialog_overlay(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        Some(self.dialog.as_ref()?.overlay(window, cx))
    }
}

impl dialogs::DialogHost for MeSidebar {
    fn dialog(&mut self) -> &mut Option<dialogs::Dialog> {
        &mut self.dialog
    }

    /// `createAndAssign` from "Create new section".
    fn create_section(&mut self, channel: String, name: String, cx: &mut Context<Self>) -> bool {
        let Some(community) = self.community(cx) else {
            return false;
        };
        if !self.model.read(cx).me.writable {
            return false;
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.toggle(&id, true, cx);
        let kit = community.read(cx).session.me_kit().clone();
        cx.background_spawn(async move {
            let _ = kit
                .me_create_and_assign(&channel, NewSection { id, name })
                .await;
        })
        .detach();
        true
    }
}

/// A quiet status line in the history (`.listMessage`).
fn list_message(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .mx(rems(0.75))
        .my(rems(0.25))
        .text_caption()
        .text_color(cx.colors().text_subtle)
        .child(text.into())
}

impl Render for MeSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let model = self.model.read(cx);
        let community = model.community.clone();
        let ready = community
            .as_ref()
            .is_some_and(|c| c.read(cx).snapshot.status == SessionStatus::Ready);
        let mut content = div()
            .id("me-conversations")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(rems(0.5))
            .text_color(c.text_standard);
        let Some(community) = community.filter(|_| ready) else {
            return div()
                .id("me-sidebar")
                .size_full()
                .child(content.child(list_message(
                    "Connect to a community to see your conversations.",
                    cx,
                )))
                .into_any_element();
        };
        self.load_collapsed(&community, cx);
        self.painted = std::mem::take(&mut *self.bounds.borrow_mut());
        let model = self.model.read(cx);
        let selection = model.selection(cx);
        let selected = selection.session().map(str::to_owned);
        let conversations = model.conversations(cx);
        let kit = model.kit.clone();
        let me = model.me.clone();
        let snapshot = community.read(cx).snapshot.clone();
        let session = community.read(cx).session.clone();

        content = content.child(self.new_conversation(cx));
        match &me.status {
            PreferencesStatus::Idle | PreferencesStatus::Loading => {
                content = content.child(list_message("Loading sections…", cx));
            }
            PreferencesStatus::Error(error) => {
                let retry = session.me_kit().clone();
                content = content.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.5))
                        .mx(rems(0.75))
                        .child(div().text_caption().text_color(c.text_danger).child(
                            if error.is_empty() {
                                "Sections couldn’t load.".to_owned()
                            } else {
                                error.clone()
                            },
                        ))
                        .child(
                            Button::new("me-retry-sections", "Retry sections")
                                .on_click(move |_, _, _| retry.me_refresh()),
                        ),
                );
            }
            PreferencesStatus::Ready => {}
        }
        if let Some(error) = &self.delete_error {
            content = content.child(list_message(error.clone(), cx).text_color(c.text_danger));
        }
        let own = Arc::new(crate::ui::sidebar::own_agents(cx));
        let shared = Shared {
            community: &community,
            snapshot: &snapshot,
            own: &own,
            selected: selected.as_deref(),
        };
        let groups = self.groups(&conversations, cx);
        // The Me groups are the viewer's projects.
        if groups.iter().any(|group| !group.id.is_empty()) {
            content = content.child(
                div()
                    .px(rems(0.5))
                    .pt(rems(0.5))
                    .text_caption()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c.text_subtle)
                    .child("PROJECTS"),
            );
        }
        for group in &groups {
            content = content.child(self.group(group, &shared, window, cx));
        }
        content = content.children(self.moves(&conversations, cx));
        match &snapshot.roster {
            RosterStatus::Loading => {
                content = content.child(list_message("Loading conversations…", cx))
            }
            RosterStatus::Error(error) => {
                let retry = session.clone();
                content = content.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.5))
                        .mx(rems(0.75))
                        .child(
                            div()
                                .text_caption()
                                .text_color(c.text_danger)
                                .child(error.clone()),
                        )
                        .child(
                            Button::new("me-retry-list", "Retry")
                                .on_click(move |_, _, _| retry.refresh_channels()),
                        ),
                );
            }
            RosterStatus::Ready => {}
        }
        if let Some((text, retry)) = super::placement_retry("me-retry-me", &kit, session.me_kit()) {
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(rems(0.5))
                    .mx(rems(0.5))
                    .my(rems(0.5))
                    .text_body()
                    .child(text)
                    .child(retry),
            );
        }
        if conversations.is_empty() && snapshot.roster == RosterStatus::Ready && kit.ready() {
            content = content.child(list_message("Start a conversation with your agents.", cx));
        }
        let dialog = self.dialog_overlay(window, cx);
        let context = self.context_menu(window, cx);
        div()
            .id("me-sidebar")
            .relative()
            .size_full()
            .child(content)
            .child(self.viewport_probe())
            .children(context)
            .children(dialog)
            .into_any_element()
    }
}
