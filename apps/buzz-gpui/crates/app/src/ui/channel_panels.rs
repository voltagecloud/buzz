//! Panels contributed to a channel (`features/panels/service.ts`,
//! `useChannelPanels.tsx`): channel tools such as Terminal or Todos, and
//! details opened from the channel menu or a link, such as channel usage.
//!
//! buzz-app's plugins register panels at run time; here each slice calls
//! [`register`] from its init. A panel can show as a tab in the channel's
//! tab pane, as a drawer under the conversation from a header launcher,
//! or both.

use crate::community::Community;
use buzz_relay_client::ChannelSummary;
use buzz_ui::IconName;
use gpui_kit::*;
use std::rc::Rc;

/// Closes the panel through its host.
pub type Close = Rc<dyn Fn(&mut Window, &mut App)>;

/// What a panel is opened with.
pub struct Opening {
    pub community: Entity<Community>,
    /// The conversation it was opened from.
    pub channel: String,
    /// The link or channel id it shows.
    pub target: String,
    pub close: Close,
    /// The root of the thread showing beside the conversation, as hex.
    pub thread: Option<String>,
    /// The thread root of the message the opening link was in, as hex.
    pub root: Option<String>,
}

/// Where a header launcher shows its panel (`channelPlacement`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A drawer under the conversation.
    Bottom,
    /// The column beside the conversation.
    Side,
}

/// A button in the channel header that toggles the panel.
#[derive(Clone, Copy)]
pub struct Launcher {
    pub icon: IconName,
    pub label: &'static str,
    pub placement: Placement,
    /// What the panel opens showing; the channel when `None`.
    pub target: Option<&'static str>,
}

/// A button in the app's header that opens the panel at `target` in the
/// companion column, on any page (`Panel.launcher`).
#[derive(Clone, Copy)]
pub struct AppLauncher {
    pub icon: IconName,
    pub label: &'static str,
    pub target: &'static str,
}

/// An entry in the channel's menu that opens the panel as a tab
/// (`channelMenu`), such as "View channel usage".
#[derive(Clone)]
pub struct MenuEntry {
    pub label: &'static str,
    /// Presentation only, not access.
    pub eligible: Eligible,
}

/// Whether a channel's menu lists a panel's entry.
pub type Eligible = Rc<dyn Fn(&ChannelSummary, &App) -> bool>;

/// Whether a panel shows a link.
pub type Matches = Rc<dyn Fn(&str) -> bool>;
/// Makes a panel's view for an opening.
pub type Build = Rc<dyn Fn(Opening, &mut Window, &mut App) -> AnyView>;

/// A panel's [`Matches`] from a closure.
pub fn matches(matches: impl Fn(&str) -> bool + 'static) -> Matches {
    Rc::new(matches)
}

/// A panel's [`Build`] from a closure.
pub fn build(build: impl Fn(Opening, &mut Window, &mut App) -> AnyView + 'static) -> Build {
    Rc::new(build)
}

#[derive(Clone)]
pub struct ChannelPanel {
    /// `plugin/panel`, unique.
    pub key: &'static str,
    /// Which of the panels that show a link wins: the lowest, then the
    /// lowest key (`MatchOrder`). By convention -10 specialises what
    /// another panel shows, 0 is the default and 100 a catch-all.
    pub order: i32,
    pub title: &'static str,
    pub icon: IconName,
    /// Links this panel shows, opened as a tab beside the conversation.
    pub matches: Matches,
    pub launcher: Option<Launcher>,
    pub app_launcher: Option<AppLauncher>,
    pub menu: Option<MenuEntry>,
    pub build: Build,
}

#[derive(Default)]
struct ChannelPanels(Vec<ChannelPanel>);

impl Global for ChannelPanels {}

/// Adds a panel, replacing one with the same key.
pub fn register(panel: ChannelPanel, cx: &mut App) {
    let panels = &mut cx.default_global::<ChannelPanels>().0;
    panels.retain(|existing| existing.key != panel.key);
    panels.push(panel);
}

/// Removes the panel `key`, as when the plugin that added it stops.
pub fn unregister(key: &str, cx: &mut App) {
    cx.default_global::<ChannelPanels>()
        .0
        .retain(|existing| existing.key != key);
}

/// A panel tab's title from its view, such as the person a profile
/// shows (`usePanelTabTitle`). Without one the tab shows the panel's
/// title.
pub type Title = fn(&AnyView, &App) -> Option<SharedString>;

#[derive(Default)]
struct Titles(Vec<(&'static str, Title)>);

impl Global for Titles {}

/// Gives a registered panel's tabs a title of their own.
pub fn register_title(key: &'static str, title: Title, cx: &mut App) {
    let titles = &mut cx.default_global::<Titles>().0;
    titles.retain(|(existing, _)| *existing != key);
    titles.push((key, title));
}

/// The tab title of `view`, a view of the panel `key`.
pub fn title(key: &str, view: &AnyView, cx: &App) -> Option<SharedString> {
    let (_, title) = cx
        .try_global::<Titles>()?
        .0
        .iter()
        .find(|(existing, _)| *existing == key)?;
    title(view, cx)
}

/// The registered panels whose plugin is on.
pub fn all(cx: &App) -> Vec<ChannelPanel> {
    cx.try_global::<ChannelPanels>()
        .map(|panels| panels.0.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|panel| crate::plugins::enabled_key(panel.key))
        .collect()
}

pub fn get(key: &str, cx: &App) -> Option<ChannelPanel> {
    all(cx).into_iter().find(|panel| panel.key == key)
}

/// The panel that shows `target`: of those that match, the lowest order,
/// then the lowest key (`resolveMatch`).
pub fn resolve(target: &str, cx: &App) -> Option<ChannelPanel> {
    winner(all(cx), target)
}

fn winner(panels: Vec<ChannelPanel>, target: &str) -> Option<ChannelPanel> {
    panels
        .into_iter()
        .filter(|panel| (panel.matches)(target))
        .min_by(|a, b| a.order.cmp(&b.order).then_with(|| a.key.cmp(b.key)))
}

/// The channel menu's panel entries for `channel`.
pub fn menu_entries(channel: &ChannelSummary, cx: &App) -> Vec<(ChannelPanel, &'static str)> {
    if channel.cached || channel.read_only {
        return Vec::new();
    }
    all(cx)
        .into_iter()
        .filter_map(|panel| {
            let menu = panel.menu.clone()?;
            (menu.eligible)(channel, cx).then_some((panel, menu.label))
        })
        .collect()
}

pub fn app_launchers(cx: &App) -> Vec<(ChannelPanel, AppLauncher)> {
    all(cx)
        .into_iter()
        .filter_map(|panel| Some((panel.clone(), panel.app_launcher?)))
        .collect()
}

pub fn launchers(cx: &App) -> Vec<(ChannelPanel, Launcher)> {
    all(cx)
        .into_iter()
        .filter_map(|panel| Some((panel.clone(), panel.launcher?)))
        .collect()
}

/// Development only: `BUZZ_DEV_CHANNEL_PANEL=1` registers a sample panel
/// with a launcher, a Tools entry and a menu entry, so headless runs can
/// screenshot the panel host before any slice registers a real panel.
pub fn dev_sample(cx: &mut App) {
    if std::env::var_os("BUZZ_DEV_CHANNEL_PANEL").is_none() {
        return;
    }
    struct Sample(Opening);
    impl Render for Sample {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let opening = &self.0;
            let close = opening.close.clone();
            div()
                .id("sample-panel")
                .p(px(20.))
                .child(format!(
                    "Sample panel in {} for {} from {}",
                    opening.community.read(cx).name(),
                    opening.target,
                    opening.channel
                ))
                .child(
                    div()
                        .id("sample-close")
                        .child("Close")
                        .on_click(move |_, window, cx| close(window, cx)),
                )
        }
    }
    register(
        ChannelPanel {
            key: "buzz.dev/sample",
            order: 0,
            title: "Sample",
            icon: IconName::ListChecks,
            matches: matches(|target| target.starts_with("buzz://dev-sample")),
            launcher: Some(Launcher {
                icon: IconName::ListChecks,
                label: "Sample",
                placement: Placement::Bottom,
                target: None,
            }),
            menu: Some(MenuEntry {
                label: "View sample",
                eligible: Rc::new(|_, _| true),
            }),
            app_launcher: None,
            build: build(|opening, _, cx| cx.new(|_| Sample(opening)).into()),
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn panel(key: &'static str, order: i32, prefix: &'static str) -> ChannelPanel {
        ChannelPanel {
            key,
            order,
            title: key,
            icon: IconName::Globe,
            matches: matches(move |target| target.starts_with(prefix)),
            launcher: None,
            menu: None,
            app_launcher: None,
            build: build(|_, _, _| unreachable!()),
        }
    }

    #[test]
    fn the_lowest_order_then_key_shows_a_link() {
        let key = |panels, target| winner(panels, target).map(|panel| panel.key);
        let all = || {
            vec![
                panel("b.web/all", 100, "https://"),
                panel("c.docs/docs", 0, "https://docs.test/"),
                panel("a.docs/docs", 0, "https://docs.test/"),
                panel("z.special/api", -10, "https://docs.test/api"),
            ]
        };
        assert_eq!(key(all(), "https://other.test/"), Some("b.web/all"));
        assert_eq!(key(all(), "https://docs.test/x"), Some("a.docs/docs"));
        assert_eq!(key(all(), "https://docs.test/api/x"), Some("z.special/api"));
        assert_eq!(key(all(), "buzz://x"), None);
    }
}
