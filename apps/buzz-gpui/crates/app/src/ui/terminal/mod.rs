//! The bundled Terminal plugin (`src/bundled/terminal`): a login shell per
//! channel in a drawer under the conversation, toggled from the channel
//! header or with Mod+J.
//!
//! Shells run on an agent host through the host protocol, like every
//! process the port starts: "This computer" (the embedded host) unless a
//! remote host is picked for the channel in the panel's header.

mod emulator;
mod keys;
mod panel;
mod sessions;
mod splash;
mod view;

use crate::ui::channel_panels::{self, ChannelPanel, Launcher, Placement};
use buzz_ui::IconName;
use gpui_kit::*;

/// `buzz.terminal`'s panel key.
pub const PANEL: &str = "buzz.terminal/terminal";

actions!(
    buzz_terminal,
    [
        /// Mod+J : shows or hides the open channel's terminal.
        ToggleTerminal,
    ]
);

pub fn init(cx: &mut App) {
    channel_panels::register(
        ChannelPanel {
            key: PANEL,
            order: 0,
            title: "Terminal",
            icon: IconName::TerminalWindow,
            matches: channel_panels::matches(|_| false),
            // Offered under Tools in the New tab picker (`isChannelTabTool`).
            launcher: Some(Launcher {
                icon: IconName::TerminalWindow,
                label: "Toggle channel terminal",
                placement: Placement::Bottom,
                target: None,
            }),
            menu: None,
            app_launcher: None,
            build: channel_panels::build(|opening, _, cx| {
                let summary = opening
                    .community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .find(|channel| channel.id == opening.channel)
                    .map(|channel| channel.name.clone())
                    .unwrap_or_else(|| opening.channel.clone());
                let context = sessions::PanelContext::new(
                    &opening.community,
                    opening.channel.clone(),
                    summary,
                    opening.thread.clone(),
                    cx,
                );
                let close = opening.close.clone();
                cx.new(|cx| panel::TerminalPanel::new(context, close, cx))
                    .into()
            }),
        },
        cx,
    );
    // Only while a channel's header shows the launcher.
    cx.on_action(|_: &ToggleTerminal, cx| {
        crate::ui::navigation::with_workspace(cx, |workspace, window, cx| {
            workspace.toggle_channel_panel(PANEL, window, cx)
        })
    });
}
