//! The Projects plugin (`buzz.projects`, buzz-app's `src/bundled/projects`):
//! a primary page for the community's NIP-34 repositories and projects,
//! entity routes for `buzz://repo|project|pr|issue` links, and the
//! composer's "Issues and pull requests" picker.

mod page;
mod resource_picker;

use super::composer_tools::{self, ComposerTool};
use super::pages::{self, Page, PageRoutes, PageSpec};
use buzz_relay_client::projects::EntityRoute;
use buzz_ui::IconName;
use gpui_kit::*;
use nostr::nips::nip19::ToBech32;

pub use page::ProjectsPage;

/// The Projects page (`buzz.projects/projects`).
pub const PAGE: Page = Page::new("buzz.projects/projects");

pub fn init(cx: &mut App) {
    pages::register(
        PageSpec::sidebar(PAGE, "Projects", IconName::FolderSimple, |window, cx| {
            cx.new(|cx| ProjectsPage::new(window, cx)).into()
        }),
        cx,
    );
    // It settles its own visits: opened once the entity loads, failed
    // with its reason (`handlesNavigation`).
    pages::register_behaviour(PAGE, true, false, true, cx);
    pages::register_routes(
        PAGE,
        PageRoutes {
            version: 1,
            validate: |params| EntityRoute::from_params(params).is_some(),
            open: |view, params, window, cx| {
                if let Ok(page) = view.clone().downcast::<ProjectsPage>() {
                    page.update(cx, |page, cx| page.open(params, window, cx));
                }
            },
        },
        cx,
    );
    composer_tools::register(
        ComposerTool {
            key: "buzz.projects/resources",
            order: 0,
            accessory: false,
            build: std::rc::Rc::new(|context, window, cx| {
                cx.new(|cx| resource_picker::ResourcePicker::new(context, window, cx))
                    .into()
            }),
        },
        cx,
    );
}

/// `formatPublicKey`: "npub…" and the last three characters. Display
/// only; never for copying, storage or routing.
pub(crate) fn format_public_key(hex: &str) -> String {
    let npub = nostr::prelude::PublicKey::from_hex(hex)
        .ok()
        .and_then(|key| key.to_bech32().ok())
        .unwrap_or_default();
    let tail: String = npub
        .chars()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("npub…{tail}")
}

/// `new Date(seconds * 1000).toLocaleString()` in en-US:
/// "10/9/2026, 3:05:09 PM".
pub(crate) fn locale_time(seconds: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(seconds as i64, 0)
        .single()
        .map(|time| time.format("%-m/%-d/%Y, %-I:%M:%S %p").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn public_keys_show_their_npub_tail() {
        let hex = "200f548dc52579907f8384eba9daaa09e485bb5cdb6f4c62c5c69a48d92f0ddb";
        assert_eq!(format_public_key(hex), "npub…7fl");
    }
}
