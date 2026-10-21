//! Settings → Messages (`channels/MessageSettings.tsx`,
//! `messages/thread-preferences.ts`): the pane new threads open in.
//!
//! The choice is this device's own default for new thread tabs, never an
//! instruction to move open ones. A choice that could not be saved stays
//! active for this run, with a notice to retry saving it.

use super::cards::{self, CardPlacement, SettingsCard};
use super::{header, preference_row, rows_group, section};
use crate::community::AppState;
use crate::ui::channel_tabs::TabPane;
use crate::ui::overlay::{self, Dismiss, select_menu};
use crate::ui::toasts;
use buzz_ui::{IconName, Menu, MenuItem, MenuLabel, Select, Surface};
use gpui_kit::*;
use std::rc::Rc;

/// Device-preference key (`buzz-thread-placement.v1`).
const PREFERENCE: &str = "thread-placement";

/// Where new threads open on this device.
#[derive(Clone, Copy, Default)]
struct ThreadPlacement {
    placement: TabPane,
    /// The choice is active but could not be saved.
    unsaved: bool,
}

impl Global for ThreadPlacement {}

/// The pane new threads open in: the side pane unless this device chose
/// the main pane.
pub fn thread_placement(cx: &App) -> TabPane {
    cx.try_global::<ThreadPlacement>()
        .map(|saved| saved.placement)
        .unwrap_or_default()
}

/// A saved choice: the main pane only when that was chosen; anything
/// else, or nothing, is the side pane.
fn saved_placement(saved: Option<&str>) -> TabPane {
    match saved {
        Some("main") => TabPane::Main,
        _ => TabPane::Side,
    }
}

/// Makes `placement` the choice, saving it on this device.
fn set_thread_placement(placement: TabPane, cx: &mut App) {
    let value = match placement {
        TabPane::Main => "main",
        TabPane::Side => "side",
    };
    let saved = cx
        .global::<AppState>()
        .store
        .set_device_pref(PREFERENCE, value);
    if let Err(error) = &saved {
        tracing::warn!(%error, "could not save the thread placement");
    }
    cx.set_global(ThreadPlacement {
        placement,
        unsaved: saved.is_err(),
    });
}

/// Restores the saved choice and registers the "Messages" card first
/// under the community (`channels/index.tsx`).
pub fn init(cx: &mut App) {
    let saved = cx.global::<AppState>().store.device_pref(PREFERENCE);
    let placement = saved_placement(saved.ok().flatten().as_deref());
    cx.set_global(ThreadPlacement {
        placement,
        unsaved: false,
    });
    cards::register(
        SettingsCard {
            key: "buzz.channels/messages",
            title: "Messages",
            icon: Some(IconName::ChatCircle),
            placement: CardPlacement::Community,
            visibility: None,
            build: super::cards::build(|_, cx| cx.new(|_| MessageSettings { menu: false }).into()),
        },
        cx,
    );
}

pub struct MessageSettings {
    menu: bool,
}

impl Render for MessageSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let saved = cx
            .try_global::<ThreadPlacement>()
            .copied()
            .unwrap_or_default();
        let label = match saved.placement {
            TabPane::Side => "Side pane",
            TabPane::Main => "Main pane",
        };
        let trigger = Select::new("thread-placement")
            .value(label)
            .surface(Surface::Floating)
            .on_click(cx.listener(|this, _, _, cx| {
                this.menu = !this.menu;
                cx.notify();
            }));
        let menu = || {
            let items = [(TabPane::Side, "Side pane"), (TabPane::Main, "Main pane")]
                .into_iter()
                .map(|(placement, label)| {
                    MenuItem::new(
                        SharedString::from(format!("thread-placement-{label}")),
                        label,
                    )
                    .checked(placement == saved.placement)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = false;
                        set_thread_placement(placement, cx);
                        cx.notify();
                    }))
                });
            Menu::new()
                .child(MenuLabel::new("Pane"))
                .children(items)
                .into_any_element()
        };
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.menu = false;
                cx.notify();
            });
        });
        let select = select_menu(
            trigger,
            self.menu,
            overlay::Align::End,
            Some(rems(10.)),
            menu,
            dismiss,
            window,
        )
        .w(rems(10.));
        let mut children = vec![
            header("Messages", None, cx).into_any_element(),
            rows_group(
                vec![
                    preference_row(
                        None,
                        "Open threads in",
                        Some(
                            "New threads open here. Already-open threads stay where you \
                             placed them."
                                .into_any_element(),
                        ),
                        vec![select.into_any_element()],
                        cx,
                    )
                    .into_any_element(),
                ],
                cx,
            )
            .into_any_element(),
        ];
        if saved.unsaved {
            let notice = toasts::Notice {
                dismissible: false,
                ..toasts::Notice::error("thread-placement", "Thread preference wasn’t saved")
            }
            .description("This choice is active, but could not be saved on this device. Try again.")
            .action(toasts::Action::new("Retry saving", move |_, cx| {
                set_thread_placement(saved.placement, cx)
            }));
            children.extend(toasts::stack(
                window,
                vec![toasts::card(&notice, 0).into_any_element()],
            ));
        }
        section(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn new_threads_open_beside_unless_the_main_pane_was_chosen() {
        assert_eq!(ThreadPlacement::default().placement, TabPane::Side);
        assert_eq!(saved_placement(None), TabPane::Side);
        assert_eq!(saved_placement(Some("side")), TabPane::Side);
        assert_eq!(saved_placement(Some("Main")), TabPane::Side);
        assert_eq!(saved_placement(Some("main")), TabPane::Main);
    }
}
