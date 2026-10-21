//! The section "More actions" menu and the row menu (ui.md §3.3, §3.5;
//! `SidebarSection.tsx`, `ChannelSidebar.tsx` `rowActions`,
//! `ChannelLifecycleMenu.tsx`, `ChannelReadMenuItem.tsx`,
//! `ChannelCopyLinkMenuItem.tsx`). The channel header's menu shows the
//! same row actions (`ChannelNavigationState.tsx` `menuActions`).

use super::{ChannelSidebar, RowMenu, SidebarProps, hidden_dms, labelled, sections};
use crate::community::Community;
use crate::ui::overlay::{self, Dismiss};
use buzz_relay_client::lifecycle::Action;
use buzz_relay_client::read_state::ReadCapability;
use buzz_relay_client::sidebar::{NewSection, SidebarSection, SortMode};
use buzz_relay_client::sidebar_store::Destination;
use buzz_relay_client::{ChannelSummary, ChannelType, SessionSnapshot};
use buzz_ui::{
    ActiveTheme, Icon, IconName, Menu, MenuItem, MenuLabel, MenuSeparator, Surface, TypeStyled,
    tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// Closes the menu a row action came from (the header's).
pub type Close = Dismiss;

/// A listener that helpers can return.
type Listener<E> = Box<dyn Fn(&E, &mut Window, &mut App)>;

/// A boxed [`Context::listener`], so helpers can return one.
fn listen<E: 'static>(
    cx: &mut Context<ChannelSidebar>,
    f: impl Fn(&mut ChannelSidebar, &E, &mut Window, &mut Context<ChannelSidebar>) + 'static,
) -> Listener<E> {
    Box::new(cx.listener(f))
}

/// `buzz://channel/<id>`, the id encoded like `encodeURIComponent`. The
/// link carries no community: it opens in the recipient's selected one.
pub fn channel_link(id: &str) -> String {
    let mut link = String::from("buzz://channel/");
    for byte in id.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => link.push(byte as char),
            _ => link.push_str(&format!("%{byte:02X}")),
        }
    }
    link
}

/// The row actions for the channel header's "Channel actions" menu:
/// move, mute, read, copy link and remove from Messages. `close` closes
/// that menu.
pub fn header_actions(
    community: &Entity<Community>,
    channel: &ChannelSummary,
    close: Close,
    cx: &mut App,
) -> Vec<AnyElement> {
    let Some(sidebar) = ChannelSidebar::of(community, cx) else {
        let link = channel_link(&channel.id);
        return vec![
            MenuItem::new("copy-link", "Copy link")
                .icon(IconName::Link)
                .icon_size(14.)
                .on_click(move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                    close(window, cx);
                })
                .into_any_element(),
        ];
    };
    sidebar.update(cx, |this, cx| {
        let snapshot = this.community.read(cx).snapshot.clone();
        let channels = labelled(&snapshot);
        let hidden = hidden_dms(&snapshot);
        let section = sections(&channels, &snapshot.sidebar, &hidden)
            .iter()
            .find(|s| s.rows.iter().any(|row| row.id == channel.id))
            .map(|s| s.key.clone())
            .unwrap_or_else(|| "channels".into());
        this.row_actions(channel, &section, Some(close), &snapshot, cx)
    })
}

/// A menu row with any leading glyph (the star, a group's emoji) and a
/// check on the current choice.
fn choice(
    id: impl Into<ElementId>,
    leading: AnyElement,
    label: impl Into<SharedString>,
    checked: bool,
    cx: &App,
) -> Stateful<Div> {
    let c = cx.colors();
    let ix = c.interaction(Surface::Floating);
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(rems(0.5))
        .min_h(rems(2.))
        .px(rems(0.75))
        .py(rems(0.5))
        .rounded(radius::FLOATING_INNER)
        .text_color(c.text_standard)
        .cursor_pointer()
        .hover(move |s| s.bg(ix.row_hover))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(14.))
                .text_color(c.text_subtle)
                .child(leading),
        )
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
        .child(div().flex_none().size(rems(1.)).when(checked, |this| {
            this.child(Icon::new(IconName::Check).size(16.))
        }))
}

impl ChannelSidebar {
    /// The section menu: Sort, Collapse or Expand, and Remove section for
    /// saved groups.
    pub(super) fn section_menu_content(
        &mut self,
        section: &SidebarSection<'_>,
        snapshot: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = section.key.clone();
        let open = self.is_open(&key);
        let sidebar = &snapshot.sidebar;
        let sort_open = self.section_menu.as_ref().is_some_and(|m| m.sort_open);
        let mode = sidebar
            .data
            .as_ref()
            .map(|data| data.sort_mode(&section.sort_key()))
            .unwrap_or_default();
        let set_sort_open = |open: bool, cx: &mut Context<Self>| {
            listen(cx, move |this, hovered: &bool, _, cx| {
                if *hovered && let Some(menu) = &mut this.section_menu {
                    menu.sort_open = open;
                    cx.notify();
                }
            })
        };
        let mut menu = Menu::new();
        if sidebar.sort_writable() {
            menu = menu.child(
                div()
                    .id("sort-trigger")
                    .on_hover(set_sort_open(true, cx))
                    .child(
                        MenuItem::new("sort", "Sort")
                            .icon(IconName::ArrowsDownUp)
                            .icon_size(14.)
                            .submenu()
                            .highlighted(sort_open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(menu) = &mut this.section_menu {
                                    menu.sort_open = !menu.sort_open;
                                    cx.notify();
                                }
                            })),
                    ),
            );
        }
        let toggle_key = key.clone();
        menu = menu.child(
            div()
                .id("toggle-trigger")
                .on_hover(set_sort_open(false, cx))
                .child(
                    MenuItem::new(
                        "toggle-section",
                        if open {
                            "Collapse section"
                        } else {
                            "Expand section"
                        },
                    )
                    .icon(if open {
                        IconName::CaretUp
                    } else {
                        IconName::CaretDown
                    })
                    .icon_size(14.)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section_menu = None;
                        this.toggle(&toggle_key, !open, cx);
                    })),
                ),
        );
        if let Some(id) = key.strip_prefix("group:")
            && sidebar.remove_section_writable()
        {
            let (id, title, count) = (id.to_owned(), section.title.clone(), section.rows.len());
            menu = menu.child(MenuSeparator::new()).child(
                div()
                    .id("remove-trigger")
                    .on_hover(set_sort_open(false, cx))
                    .child(
                        MenuItem::new("remove-section", "Remove section")
                            .icon(IconName::Trash)
                            .icon_size(14.)
                            .danger()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.section_menu = None;
                                this.dialog = Some(super::dialogs::Dialog::RemoveSection {
                                    id: id.clone(),
                                    title: title.clone(),
                                    count,
                                    busy: false,
                                    error: None,
                                });
                                cx.notify();
                            })),
                    ),
            );
        }
        let sort_item = |id: &'static str, label: &'static str, icon: IconName, value: SortMode| {
            let key = key.clone();
            choice(
                id,
                Icon::new(icon).size(14.).into_any_element(),
                label,
                mode == value,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if mode != value {
                    this.set_sort(&key, value, cx);
                } else {
                    this.close_menus(cx);
                }
            }))
        };
        div()
            .flex()
            .items_start()
            .gap(px(4.))
            .child(menu)
            .when(sort_open, |this| {
                this.child(
                    Menu::new()
                        .child(sort_item(
                            "sort-recent",
                            "Recent",
                            IconName::Timer,
                            SortMode::Recent,
                        ))
                        .child(sort_item(
                            "sort-alpha",
                            "A–Z",
                            IconName::TextAa,
                            SortMode::Alpha,
                        )),
                )
            })
            .into_any_element()
    }

    /// The actions a row offers, in buzz-app's order. `surface` is the
    /// header menu the actions show in instead of the row menu.
    pub(super) fn row_actions(
        &mut self,
        channel: &ChannelSummary,
        section: &str,
        surface: Option<Close>,
        snapshot: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let c = *cx.colors();
        let sidebar = &snapshot.sidebar;
        let id = channel.id.clone();
        let dm = channel.channel_type == Some(ChannelType::Dm);
        let menu_open = self
            .row_menu
            .as_ref()
            .is_some_and(|m| m.move_open && !m.surface);
        let close_move = |cx: &mut Context<Self>| {
            listen(cx, |this, hovered: &bool, _, cx| {
                if *hovered
                    && let Some(menu) = &mut this.row_menu
                    && menu.move_open
                    && !menu.surface
                {
                    menu.move_open = false;
                    cx.notify();
                }
            })
        };
        let mut actions: Vec<AnyElement> = Vec::new();
        let item = |id: &'static str, label: &'static str, icon: IconName| {
            MenuItem::new(id, label).icon(icon).icon_size(14.)
        };
        let placement = sidebar.writable && sidebar.data.is_some();
        if placement && channel.channel_type != Some(ChannelType::Forum) {
            let label = if dm {
                "Move conversation"
            } else {
                "Move channel"
            };
            let (target, key) = (channel.clone(), section.to_owned());
            let entry = match &surface {
                Some(close) => {
                    let close = close.clone();
                    item("move", label, IconName::FolderSimple)
                        .submenu()
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            close(window, cx);
                            this.open_row_menu(
                                RowMenu::new(target.clone(), key.clone(), event.position())
                                    .header_move(),
                                window,
                                cx,
                            );
                        }))
                }
                None => item("move", label, IconName::FolderSimple)
                    .submenu()
                    .highlighted(menu_open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(menu) = &mut this.row_menu {
                            menu.move_open = !menu.move_open;
                            cx.notify();
                        }
                    })),
            };
            actions.push(
                div()
                    .id("move-trigger")
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if *hovered
                            && let Some(menu) = &mut this.row_menu
                            && !menu.surface
                        {
                            menu.move_open = true;
                            cx.notify();
                        }
                    }))
                    .child(entry)
                    .into_any_element(),
            );
        }
        let pending = self.read_action.pending;
        if sidebar.mute_writable() {
            let muted = self.muted(&id, cx);
            let (channel_id, name) = (id.clone(), channel.name.clone());
            let close = surface.clone();
            actions.push(
                div()
                    .id("mute-trigger")
                    .on_hover(close_move(cx))
                    .child(
                        item(
                            "mute",
                            if muted { "Unmute" } else { "Mute" },
                            if muted {
                                IconName::Bell
                            } else {
                                IconName::BellSlash
                            },
                        )
                        .disabled(pending)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if let Some(close) = &close {
                                    close(window, cx);
                                }
                                this.change_mute(&channel_id, &name, !muted, cx)
                            },
                        )),
                    )
                    .into_any_element(),
            );
        }
        let readable = snapshot
            .unread
            .sync
            .as_ref()
            .is_some_and(|sync| sync.capability == ReadCapability::FrontierSync);
        if readable {
            let unread = snapshot.unread.channel(&id).unread;
            let (channel_id, close) = (id.clone(), surface.clone());
            let read = item(
                "read",
                if unread {
                    "Mark as Read"
                } else {
                    "Mark as Unread"
                },
                if unread {
                    IconName::EnvelopeOpen
                } else {
                    IconName::Envelope
                },
            )
            .disabled(pending)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.run_read(&channel_id, unread, close.clone(), window, cx)
            }));
            actions.push(
                div()
                    .id("read-trigger")
                    .on_hover(close_move(cx))
                    .when(!unread, |this| {
                        this.tooltip(buzz_ui::Tooltip::text("Mark unread on this device only"))
                    })
                    .child(read)
                    .into_any_element(),
            );
        }
        let (channel_id, close) = (id.clone(), surface.clone());
        actions.push(
            div()
                .id("copy-trigger")
                .on_hover(close_move(cx))
                .child(
                    item("copy-link", "Copy link", IconName::Link).on_click(cx.listener(
                        move |this, _, window, cx| {
                            if let Some(close) = &close {
                                close(window, cx);
                            }
                            this.copy_link(&channel_id, cx)
                        },
                    )),
                )
                .into_any_element(),
        );
        if surface.is_none() && !channel.archived {
            actions.extend(self.lifecycle_items(channel, cx));
        }
        if dm {
            let (channel_id, close) = (id.clone(), surface.clone());
            actions.push(
                div()
                    .id("hide-trigger")
                    .on_hover(close_move(cx))
                    .child(
                        item("remove-message", "Remove from Messages", IconName::Minus).on_click(
                            cx.listener(move |this, _, window, cx| {
                                if let Some(close) = &close {
                                    close(window, cx);
                                }
                                this.hide_dm(&channel_id, cx)
                            }),
                        ),
                    )
                    .into_any_element(),
            );
        }
        if self.read_action.pending {
            actions.push(MenuLabel::new("Saving…").quiet().into_any_element());
        }
        if let Some(error) = &self.read_action.error {
            actions.push(
                div()
                    .px(rems(0.5))
                    .py(rems(0.25))
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        actions
    }

    /// Hide, or Leave / Archive / Delete, once permissions are read
    /// (`ChannelLifecycleMenu.tsx`).
    fn lifecycle_items(
        &mut self,
        channel: &ChannelSummary,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(menu) = &self.row_menu else {
            return Vec::new();
        };
        let item = |id: &'static str, label: &'static str, icon: IconName| {
            MenuItem::new(id, label).icon(icon).icon_size(14.)
        };
        let busy = self.dialog.is_some();
        let choose = |action: Action, cx: &mut Context<Self>| {
            let channel = channel.clone();
            listen(cx, move |this, _: &ClickEvent, window, cx| {
                this.choose_lifecycle(channel.clone(), action, window, cx)
            })
        };
        let retry = |cx: &mut Context<Self>| {
            listen(cx, |this, _: &ClickEvent, window, cx| {
                if let Some(menu) = this.row_menu.take() {
                    this.open_row_menu(
                        RowMenu::new(menu.channel, menu.section, menu.at),
                        window,
                        cx,
                    );
                }
            })
        };
        let wrap = |id: &'static str, element: AnyElement, cx: &mut Context<Self>| {
            div()
                .id(id)
                .on_hover(listen(cx, |this, hovered: &bool, _, cx| {
                    if *hovered && let Some(menu) = &mut this.row_menu {
                        menu.move_open = false;
                        cx.notify();
                    }
                }))
                .child(element)
                .into_any_element()
        };
        let mut items = Vec::new();
        match &menu.permissions {
            None => {}
            Some(Err(_)) => {
                items.push(MenuLabel::new("Channel actions unavailable").into_any_element());
                items.push(wrap(
                    "retry-permissions",
                    item(
                        "retry-permissions-item",
                        "Retry channel permissions",
                        IconName::ArrowClockwise,
                    )
                    .disabled(busy)
                    .on_click(retry(cx))
                    .into_any_element(),
                    cx,
                ));
            }
            Some(Ok(p)) if p.can_hide => {
                items.push(wrap(
                    "hide",
                    item("hide-item", "Hide conversation", IconName::EyeSlash)
                        .disabled(busy)
                        .on_click(choose(Action::Hide, cx))
                        .into_any_element(),
                    cx,
                ));
            }
            Some(Ok(p)) => {
                let p = p.clone();
                if p.can_leave {
                    items.push(wrap(
                        "leave",
                        item("leave-item", "Leave channel", IconName::SignOut)
                            .disabled(busy)
                            .on_click(choose(Action::Leave, cx))
                            .into_any_element(),
                        cx,
                    ));
                }
                if p.can_archive || p.can_unarchive {
                    let (label, action) = if p.can_unarchive {
                        ("Unarchive channel", Action::Unarchive)
                    } else {
                        ("Archive channel", Action::Archive)
                    };
                    items.push(wrap(
                        "archive",
                        item("archive-item", label, IconName::Archive)
                            .disabled(busy)
                            .on_click(choose(action, cx))
                            .into_any_element(),
                        cx,
                    ));
                }
                if p.delete_unavailable {
                    items.push(
                        item(
                            "delete-unavailable",
                            "Delete check unavailable",
                            IconName::WarningCircle,
                        )
                        .disabled(true)
                        .into_any_element(),
                    );
                    items.push(wrap(
                        "retry-delete",
                        item(
                            "retry-delete-item",
                            "Retry Delete check",
                            IconName::ArrowClockwise,
                        )
                        .disabled(busy)
                        .on_click(retry(cx))
                        .into_any_element(),
                        cx,
                    ));
                }
                if p.can_delete {
                    items.push(wrap(
                        "delete",
                        item("delete-item", "Delete channel", IconName::Trash)
                            .danger()
                            .disabled(busy)
                            .on_click(choose(Action::Delete, cx))
                            .into_any_element(),
                        cx,
                    ));
                }
            }
        }
        items
    }

    /// The Move submenu: Starred, the saved groups, Direct messages for
    /// DMs; then Create new… and Remove from the current section.
    fn move_submenu(
        &mut self,
        channel: &ChannelSummary,
        section: &str,
        snapshot: &SessionSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dm = channel.channel_type == Some(ChannelType::Dm);
        let starred = section == "starred";
        let current_group = section.strip_prefix("group:").map(str::to_owned);
        let current = if starred {
            "starred".to_owned()
        } else if let Some(group) = &current_group {
            format!("group:{group}")
        } else if dm {
            "dms".to_owned()
        } else {
            "channels".to_owned()
        };
        let id = channel.id.clone();
        let pick = |value: String, destination: Destination, cx: &mut Context<Self>| {
            let (id, current) = (id.clone(), current.clone());
            listen(cx, move |this, _: &ClickEvent, _, cx| {
                if value == current {
                    this.close_menus(cx);
                } else {
                    this.move_channel(&id, destination.clone(), cx);
                }
            })
        };
        let mut menu = Menu::new().child(MenuLabel::new("Move to…")).child(
            choice(
                "to-starred",
                div().child("★").into_any_element(),
                "Starred",
                current == "starred",
                cx,
            )
            .on_click(pick("starred".into(), Destination::Starred, cx)),
        );
        let groups = snapshot
            .sidebar
            .data
            .as_ref()
            .map(|data| data.sections.clone())
            .unwrap_or_default();
        for group in groups {
            let value = format!("group:{}", group.id);
            let leading = match &group.icon {
                Some(icon) => crate::ui::status::emoji(icon, px(13.), &self.community, cx),
                None => None,
            }
            .unwrap_or_else(|| {
                Icon::new(IconName::FolderSimple)
                    .size(14.)
                    .into_any_element()
            });
            menu = menu.child(
                choice(
                    SharedString::from(format!("to-{value}")),
                    leading,
                    group.name.clone(),
                    current == value,
                    cx,
                )
                .on_click(pick(
                    value,
                    Destination::Section(Some(group.id.clone())),
                    cx,
                )),
            );
        }
        if dm {
            menu = menu.child(
                choice(
                    "to-dms",
                    Icon::new(IconName::ChatCircle).size(14.).into_any_element(),
                    "Direct messages",
                    current == "dms",
                    cx,
                )
                .on_click(pick("dms".into(), Destination::Section(None), cx)),
            );
        }
        let target = channel.clone();
        menu = menu.child(MenuSeparator::new()).child(
            MenuItem::new("create-section", "Create new…")
                .icon(IconName::Plus)
                .icon_size(14.)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_menus(cx);
                    this.open_create_section(target.clone(), window, cx);
                })),
        );
        if starred || current_group.is_some() {
            let title = snapshot
                .sidebar
                .data
                .as_ref()
                .and_then(|data| {
                    current_group
                        .as_ref()
                        .and_then(|g| data.sections.iter().find(|s| s.id == *g))
                        .map(|s| s.name.clone())
                })
                .unwrap_or_else(|| "Starred".into());
            let id = channel.id.clone();
            menu = menu.child(
                MenuItem::new("remove-from", format!("Remove from {title}"))
                    .icon(IconName::Minus)
                    .icon_size(14.)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.move_channel(&id, Destination::Section(None), cx)
                    })),
            );
        }
        menu.into_any_element()
    }

    /// The open row menu, at the pointer or under the focused row.
    pub(super) fn row_menu_overlay(
        &mut self,
        snapshot: &SessionSnapshot,
        _: &SidebarProps,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.row_menu.as_ref()?;
        let (channel, section, at, move_open, surface) = (
            menu.channel.clone(),
            menu.section.clone(),
            menu.at,
            menu.move_open,
            menu.surface,
        );
        let actions = if surface {
            Vec::new()
        } else {
            self.row_actions(&channel, &section, None, snapshot, cx)
        };
        let submenu = move_open.then(|| self.move_submenu(&channel, &section, snapshot, cx));
        let content = div()
            .flex()
            .items_start()
            .gap(px(4.))
            .when(!surface, |this| this.child(Menu::new().children(actions)))
            .children(submenu);
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.close_menus(cx));
        });
        Some(overlay::context_menu(window, at, content, dismiss).into_any_element())
    }

    pub(super) fn open_create_section(
        &mut self,
        channel: ChannelSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dialog = Some(super::dialogs::Dialog::create_section(channel, window, cx));
        cx.notify();
    }

    /// "Create and move": a new group named `name`, the channel moved in.
    pub(super) fn create_section(
        &mut self,
        channel: &str,
        section: NewSection,
        cx: &mut Context<Self>,
    ) {
        self.move_channel(channel, Destination::Create(section), cx);
    }
}
