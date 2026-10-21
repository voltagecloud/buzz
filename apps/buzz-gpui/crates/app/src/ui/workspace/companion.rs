//! The companion dock (`PanelFrame.tsx`, `usePanelLauncher.ts`): a panel
//! the shell opens beside any page, such as the viewer's own profile from
//! the account menu.

use super::Workspace;
use super::layout::{self, Split};
use crate::ui::channel_panels;
use crate::ui::pages::{self, Page};
use crate::ui::panels::{self, Dock};
use crate::ui::profile_panel::{self, ProfilePanel};
use buzz_ui::IconName;
use gpui_kit::*;
use std::rc::Rc;

/// What the companion dock shows.
#[derive(Clone)]
pub(super) enum Companion {
    /// A person's profile; the account menu opens the viewer's.
    Profile(Entity<ProfilePanel>),
    /// A channel panel opened beside any page, from the header's launcher
    /// or for a target outside a conversation.
    Panel {
        key: &'static str,
        title: &'static str,
        icon: IconName,
        view: AnyView,
    },
}

impl Workspace {
    /// "View your profile": the viewer's profile beside the page.
    pub(super) fn open_own_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = &self.community else {
            return;
        };
        let pubkey = community.read(cx).session.viewer().to_string();
        self.open_profile(pubkey, window, cx);
    }

    /// Shows `pubkey`'s profile panel in the companion dock.
    pub(super) fn open_profile(
        &mut self,
        pubkey: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(community) = self.community.clone() else {
            return;
        };
        let showing = matches!(&self.companion, Some(Companion::Profile(view))
            if view.read(cx).pubkey() == pubkey);
        if !showing {
            let weak = cx.entity().downgrade();
            let close: crate::ui::channel_panels::Close = Rc::new(move |window, cx| {
                let _ = weak.update(cx, |this, cx| this.close_companion(window, cx));
            });
            let view = profile_panel::view(community, pubkey, close, window, cx);
            self.companion = Some(Companion::Profile(view));
            self.companion_opening += 1;
        }
        cx.notify();
    }

    /// Shows panel `key`, or the panel that shows `target` when `None`,
    /// beside the page, opened at `target`.
    pub(crate) fn open_companion_panel(
        &mut self,
        key: Option<&str>,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(community) = self.community.clone() else {
            return false;
        };
        let panel = match key {
            Some(key) => channel_panels::get(key, cx),
            None => channel_panels::resolve(target, cx),
        };
        let Some(panel) = panel else {
            return false;
        };
        // Beside the page it belongs to no conversation (the shell's
        // `PanelCard` has no `context`).
        let channel = String::new();
        let weak = cx.entity().downgrade();
        // Closes this opening only, not a panel that replaced it.
        let opening = self.companion_opening + 1;
        let close: channel_panels::Close = Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| {
                if this.companion_opening == opening {
                    this.close_companion(window, cx);
                }
            });
        });
        let view = (panel.build)(
            channel_panels::Opening {
                community,
                channel,
                target: target.to_owned(),
                close,
                thread: None,
                root: None,
            },
            window,
            cx,
        );
        self.companion = Some(Companion::Panel {
            key: panel.key,
            title: panel.title,
            icon: panel.icon,
            view,
        });
        self.companion_opening += 1;
        cx.notify();
        true
    }

    /// A header launcher: opens its panel beside the page, or closes it
    /// when it shows.
    pub(super) fn toggle_app_launcher(
        &mut self,
        key: &'static str,
        target: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&self.companion, Some(Companion::Panel { key: open, .. }) if *open == key) {
            self.close_companion(window, cx);
        } else {
            self.open_companion_panel(Some(key), target, window, cx);
        }
    }

    /// The panel the companion shows, if any.
    pub(super) fn companion_key(&self) -> Option<&'static str> {
        match &self.companion {
            Some(Companion::Panel { key, .. }) => Some(key),
            _ => None,
        }
    }

    /// Closing returns focus to the account menu's trigger, which opened it.
    /// Closes the companion if it shows panel `key`.
    pub(super) fn withdraw_companion(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&self.companion, Some(Companion::Panel { key: open, .. }) if *open == key) {
            self.close_companion(window, cx);
        }
    }

    pub(super) fn close_companion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.companion.take().is_some() {
            window.focus(&self.main_focus, cx);
            cx.notify();
        }
    }

    /// The companion's card, if one is open.
    pub(super) fn companion_panel(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let weak = cx.entity().downgrade();
        let close: panels::Close = Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.close_companion(window, cx));
        });
        Some(match self.companion.clone()? {
            Companion::Profile(view) => panels::card(
                "companion",
                "Profile",
                None,
                view.into_any_element(),
                close,
                cx,
            ),
            Companion::Panel {
                title, icon, view, ..
            } => panels::card(
                "companion",
                title,
                Some(icon),
                view.into_any_element(),
                close,
                cx,
            ),
        })
    }

    /// The page region's width: the joined panel less the sidebar beside
    /// it.
    pub(super) fn main_width(&self, window: &Window, cx: &App) -> f32 {
        let width = f32::from(window.viewport_size().width);
        let content = Self::content_width(window);
        let collapsible = self.page == Page::SETTINGS
            || pages::spec(self.page, cx).is_some_and(|spec| spec.collapsible);
        let has_sidebar = self.page == Page::SETTINGS
            || self.own_sidebars.contains_key(&self.page)
            || self.community.is_some();
        let beside = !layout::narrow(width) && has_sidebar && (self.sidebar_open || !collapsible);
        match beside {
            true => content - self.sidebar_width(window),
            false => content,
        }
    }

    /// A page other than Messages with the companion dock beside it.
    pub(super) fn frame(
        &mut self,
        page: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Every page shows the dock: one that does not place the companion
        // itself gets the shell's (`pageOwnsCompanion`).
        let companion = self.companion_panel(window, cx);
        let weak = cx.entity().downgrade();
        Dock {
            id: "companion".into(),
            split: Split::COMPANION,
            preferred: self.companion_width,
            window: f32::from(window.viewport_size().width),
            available: self.main_width(window, cx),
            opening: self.companion_opening,
            on_resize: Rc::new(move |width, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.companion_width = width;
                    cx.notify();
                });
            }),
            on_commit: Rc::new(|_, _| {}),
        }
        .render(page, companion, cx)
    }
}
