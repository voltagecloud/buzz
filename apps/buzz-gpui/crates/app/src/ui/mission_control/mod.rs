//! The Mission control page (`buzz.mission-control`): every agent on
//! every host in one live list, the ones that need their owner first, with
//! a summary of each host above it. The model is
//! [`crate::mission_control`]; this is how it is drawn and acted on.

mod page;
mod row;

use super::navigation::{self, Place, Target};
use super::pages::{self, Page, PageSpec};
use crate::community::AppState;
use crate::mission_control::Fleet;
use buzz_ui::{ActiveTheme, IconName};
use gpui_kit::*;

pub const PAGE: Page = Page::new("buzz.mission-control/mission-control");

actions!(
    buzz_mission_control,
    [
        /// Mod+Shift+M : the Mission control page.
        OpenMissionControl,
    ]
);

/// Registers the page and its shortcut's action.
pub fn register(cx: &mut App) {
    pages::register(
        PageSpec {
            badge: Some(std::rc::Rc::new(badge)),
            ..PageSpec::sidebar(PAGE, "Mission control", IconName::SquaresFour, |_, cx| {
                cx.new(page::MissionControlPage::new).into()
            })
        },
        cx,
    );
    cx.on_action(|_: &OpenMissionControl, cx| {
        navigation::with_workspace(cx, |workspace, window, cx| {
            let community = AppState::current(cx).map(|c| c.read(cx).origin.to_string());
            workspace.navigate(
                Target {
                    community,
                    place: Place::Page(PAGE),
                },
                window,
                cx,
            )
        })
    });
}

/// The sidebar row's count of agents needing attention.
fn badge(cx: &App) -> Option<AnyElement> {
    let count = Fleet::global(cx)?.read(cx).board().attention();
    (count > 0).then(|| {
        div()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.colors().text_danger)
            .child(count.to_string())
            .into_any_element()
    })
}
