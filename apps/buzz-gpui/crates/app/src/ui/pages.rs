//! Primary pages (`features/pages`, `app/shell/presentation.ts`).
//!
//! buzz-app's plugins contribute pages at run time. Here each page module
//! registers itself from [`init`]: Me and Messages go to the header pills,
//! the rest become rows at the top of the channel sidebar. The shell draws
//! the rows, lists the pages in search and records them in history, so a
//! new page needs only its [`PageSpec`] and a line in [`init`].

use buzz_ui::IconName;
use gpui_kit::*;
use std::cmp::Ordering;
use std::rc::Rc;

/// A page's identity: buzz-app's contribution key, such as
/// `buzz.inbox/inbox`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Page(&'static str);

impl Page {
    pub const ME: Page = Page("buzz.me/me");
    pub const MESSAGES: Page = Page("buzz.channels/channels");
    pub const AGENTS: Page = Page("buzz.agents/agents");
    /// Settings is shell-owned: never a primary page, but a place.
    pub const SETTINGS: Page = Page("settings");

    pub const fn new(key: &'static str) -> Self {
        Self(key)
    }

    pub fn key(self) -> &'static str {
        self.0
    }
}

/// Where a primary page's entry sits (`Page.placement`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A pill in the middle of the header.
    Topbar,
    /// A row at the top of the channel sidebar.
    Sidebar,
    /// No navigation entry (`primary: false`); still in search.
    Hidden,
}

/// Which sidebar the shell shows beside a page.
#[derive(Clone, Copy)]
pub enum Navigation {
    /// The channel sidebar, with the page rows at its top.
    Channels,
    /// The page's own sidebar, such as Me's history. It gets the saved
    /// channel sidebar width but cannot resize it.
    Own(fn(&mut Window, &mut App) -> AnyView),
}

/// The view a page shows. `None` for Messages, which the workspace draws.
pub type Build = Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;

/// A page's [`Build`] from a closure.
pub fn build(build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Option<Build> {
    Some(Rc::new(build))
}

/// A trailing count or mark on the page's row, such as Reminders' due
/// count. `None` hides it.
pub type Badge = Rc<dyn Fn(&App) -> Option<AnyElement>>;

#[derive(Clone)]
pub struct PageSpec {
    pub page: Page,
    pub label: &'static str,
    pub icon: IconName,
    pub placement: Placement,
    pub navigation: Navigation,
    /// The header toggle collapses the sidebar even with no navigation
    /// entry; every primary page and Settings have it.
    pub collapsible: bool,
    pub badge: Option<Badge>,
    pub build: Option<Build>,
}

impl PageSpec {
    /// A page shown as a sidebar row beside the channel sidebar, the
    /// usual case for bundled pages.
    pub fn sidebar(
        page: Page,
        label: &'static str,
        icon: IconName,
        build: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        Self {
            page,
            label,
            icon,
            placement: Placement::Sidebar,
            navigation: Navigation::Channels,
            collapsible: false,
            badge: None,
            build: self::build(build),
        }
    }
}

/// The registered pages, in registration order.
#[derive(Default)]
pub struct Pages(Vec<PageSpec>);

impl Global for Pages {}

/// A page that takes navigation targets of its own (`handlesNavigation`
/// with `route.validate`), such as Projects' entity routes. History
/// records each route as a place.
#[derive(Clone, Copy)]
pub struct PageRoutes {
    /// What the params mean (`route.version`); a target made for another
    /// version is unavailable.
    pub version: u32,
    /// Whether `params` name a place the page can open.
    pub validate: fn(&serde_json::Value) -> bool,
    /// Shows `params` in the page's view, or the page's start for `None`.
    pub open: fn(&AnyView, Option<&serde_json::Value>, &mut Window, &mut App),
}

#[derive(Default)]
struct Routes(std::collections::HashMap<Page, PageRoutes>);

impl Global for Routes {}

/// Pages drawn with an image of their own rather than an icon, such as a
/// plugin's `data:image/` icon.
#[derive(Default)]
struct Images(std::collections::HashMap<Page, ImageSource>);

impl Global for Images {}

/// Pages that say when a visit to them has opened or failed
/// (`Page.handlesNavigation`), and pages shown without the companion dock
/// (a plugin page that does not opt in to `companion`).
#[derive(Default)]
struct Behaviour {
    settles: std::collections::HashSet<Page>,
    solo: std::collections::HashSet<Page>,
    edge_to_edge: std::collections::HashSet<Page>,
}

impl Global for Behaviour {}

/// Marks `page` as settling its own visits, and as showing the companion
/// dock or not.
pub fn register_behaviour(
    page: Page,
    settles: bool,
    companion: bool,
    edge_to_edge: bool,
    cx: &mut App,
) {
    let behaviour = cx.default_global::<Behaviour>();
    if edge_to_edge {
        behaviour.edge_to_edge.insert(page);
    }
    if settles {
        behaviour.settles.insert(page);
    }
    if !companion {
        behaviour.solo.insert(page);
    }
}

/// Whether a visit to `page` stays opening until the page settles it.
pub fn settles(page: Page, cx: &App) -> bool {
    cx.try_global::<Behaviour>()
        .is_some_and(|behaviour| behaviour.settles.contains(&page))
}

/// Whether `page` fills the area edge to edge (`layout: "workspace"`).
pub fn edge_to_edge(page: Page, cx: &App) -> bool {
    cx.try_global::<Behaviour>()
        .is_some_and(|behaviour| behaviour.edge_to_edge.contains(&page))
}

/// Whether `page` shows without the companion dock.
pub fn solo(page: Page, cx: &App) -> bool {
    cx.try_global::<Behaviour>()
        .is_some_and(|behaviour| behaviour.solo.contains(&page))
}

/// Draws `page` with `image` wherever it shows its icon.
pub fn register_image(page: Page, image: ImageSource, cx: &mut App) {
    cx.default_global::<Images>().0.insert(page, image);
}

/// `page`'s own image, 16px, for the places that show its icon.
pub fn image(page: Page, cx: &App) -> Option<AnyElement> {
    let image = cx.try_global::<Images>()?.0.get(&page)?.clone();
    Some(
        img(image)
            .size(px(16.))
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
    )
}

/// Lets `page` handle navigation targets.
pub fn register_routes(page: Page, routes: PageRoutes, cx: &mut App) {
    cx.default_global::<Routes>().0.insert(page, routes);
}

pub fn routes(page: Page, cx: &App) -> Option<PageRoutes> {
    cx.try_global::<Routes>()?.0.get(&page).copied()
}

/// Registers the bundled pages. A new page adds its `register` call here.
pub fn init(cx: &mut App) {
    cx.set_global(Pages::default());
    register(
        PageSpec {
            page: Page::MESSAGES,
            label: "Messages",
            icon: IconName::ChatsCircle,
            placement: Placement::Topbar,
            navigation: Navigation::Channels,
            collapsible: true,
            badge: None,
            build: None,
        },
        cx,
    );
    // `buzz.channels/channels` route v1 (`isChannelRoute`): the window
    // shows these itself, as a channel route.
    register_routes(
        Page::MESSAGES,
        PageRoutes {
            version: 1,
            validate: |params| super::navigation::ChannelRoute::parse(params).is_some(),
            open: |_, _, _, _| {},
        },
        cx,
    );
    super::me::register(cx);
    super::inbox::register(cx);
    super::workflows::register(cx);
    super::agents2::register(cx);
    super::mission_control::register(cx);
}

/// Adds a page, replacing any earlier one with the same key.
pub fn register(spec: PageSpec, cx: &mut App) {
    let pages = &mut cx.default_global::<Pages>().0;
    pages.retain(|existing| existing.page != spec.page);
    pages.push(spec);
}

/// Drops `page`'s route handler and image before it is registered again,
/// so a page that no longer has them does not keep the old ones.
pub fn clear_extras(page: Page, cx: &mut App) {
    cx.default_global::<Routes>().0.remove(&page);
    cx.default_global::<Images>().0.remove(&page);
    let behaviour = cx.default_global::<Behaviour>();
    behaviour.settles.remove(&page);
    behaviour.solo.remove(&page);
    behaviour.edge_to_edge.remove(&page);
}

/// Removes the page `page`, as when the plugin that added it stops.
pub fn unregister(page: Page, cx: &mut App) {
    cx.default_global::<Pages>()
        .0
        .retain(|existing| existing.page != page);
    clear_extras(page, cx);
}

/// A registered page whose plugin is on.
pub fn spec(page: Page, cx: &App) -> Option<PageSpec> {
    cx.try_global::<Pages>()?
        .0
        .iter()
        .find(|spec| spec.page == page && crate::plugins::enabled_key(page.key()))
        .cloned()
}

/// Every registered page whose plugin is on, in host order.
pub fn all(cx: &App) -> Vec<PageSpec> {
    let mut pages: Vec<PageSpec> = cx
        .try_global::<Pages>()
        .map(|pages| pages.0.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|spec| crate::plugins::enabled_key(spec.page.key()))
        .collect();
    pages.sort_by(|a, b| order(a.page, a.label, b.page, b.label));
    pages
}

/// The registered pages placed at `placement`, in host order.
pub fn placed(placement: Placement, cx: &App) -> Vec<PageSpec> {
    all(cx)
        .into_iter()
        .filter(|spec| spec.placement == placement)
        .collect()
}

/// The header pill `page` selects (`headerPageSelection`). Pages beside
/// the channel sidebar sit inside Messages, so its pill stays selected on
/// Inbox or Agents while their sidebar row and route stay exact. Me,
/// Settings, header pages and unregistered pages select only themselves.
pub fn header_selection(page: Page, cx: &App) -> Page {
    area(page, &all(cx))
}

fn area(page: Page, pages: &[PageSpec]) -> Page {
    let beside_channels = pages
        .iter()
        .any(|spec| spec.page == page && spec.placement != Placement::Topbar);
    let messages = pages
        .iter()
        .any(|spec| spec.page == Page::MESSAGES && spec.placement == Placement::Topbar);
    if page != Page::ME && beside_channels && messages {
        Page::MESSAGES
    } else {
        page
    }
}

/// Bundled pages keep fixed slots; the rest follow by label, then by key,
/// so the order never depends on registration timing (`orderPages`).
const BUNDLED_ORDER: [Page; 6] = [
    Page::ME,
    Page::MESSAGES,
    Page::new("buzz.inbox/inbox"),
    Page::new("buzz.reminders/reminders"),
    Page::new("buzz.bestie/bestie"),
    Page::new("buzz.projects/projects"),
];

fn order(a: Page, a_label: &str, b: Page, b_label: &str) -> Ordering {
    let rank = |page: Page| {
        BUNDLED_ORDER
            .iter()
            .position(|slot| *slot == page)
            .unwrap_or(BUNDLED_ORDER.len())
    };
    rank(a)
        .cmp(&rank(b))
        .then_with(|| a_label.to_lowercase().cmp(&b_label.to_lowercase()))
        .then_with(|| a.key().cmp(b.key()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn sorted(mut pages: Vec<(Page, &'static str)>) -> Vec<&'static str> {
        pages.sort_by(|a, b| order(a.0, a.1, b.0, b.1));
        pages.into_iter().map(|(_, label)| label).collect()
    }

    #[test]
    fn bundled_pages_keep_their_slots() {
        let pages = vec![
            (Page::new("buzz.workflows/workflows"), "Workflows"),
            (Page::AGENTS, "Agents"),
            (Page::new("buzz.projects/projects"), "Projects"),
            (Page::new("buzz.inbox/inbox"), "Inbox"),
            (Page::MESSAGES, "Messages"),
            (Page::new("buzz.reminders/reminders"), "Reminders"),
            (Page::ME, "Me"),
        ];
        assert_eq!(
            sorted(pages),
            [
                "Me",
                "Messages",
                "Inbox",
                "Reminders",
                "Projects",
                "Agents",
                "Workflows"
            ]
        );
    }

    fn placed(page: &'static str, placement: Placement) -> PageSpec {
        PageSpec {
            page: Page::new(page),
            label: page,
            icon: IconName::Browser,
            placement,
            navigation: Navigation::Channels,
            collapsible: false,
            badge: None,
            build: None,
        }
    }

    fn registered() -> Vec<PageSpec> {
        vec![
            placed("buzz.me/me", Placement::Topbar),
            placed("buzz.channels/channels", Placement::Topbar),
            placed("buzz.inbox/inbox", Placement::Sidebar),
            placed("buzz.reminders/reminders", Placement::Sidebar),
            placed("buzz.agents/agents", Placement::Sidebar),
            placed("buzz.workflows/workflows", Placement::Sidebar),
            placed("buzz.sessions/sessions", Placement::Hidden),
            placed("example.sidebar/me", Placement::Sidebar),
            placed("example.header/main", Placement::Topbar),
        ]
    }

    #[test]
    fn pages_beside_the_channel_sidebar_keep_messages_selected() {
        let pages = registered();
        for (selected, expected) in [
            ("buzz.channels/channels", "buzz.channels/channels"),
            ("buzz.inbox/inbox", "buzz.channels/channels"),
            ("buzz.reminders/reminders", "buzz.channels/channels"),
            ("buzz.agents/agents", "buzz.channels/channels"),
            ("buzz.workflows/workflows", "buzz.channels/channels"),
            ("buzz.sessions/sessions", "buzz.channels/channels"),
            ("example.sidebar/me", "buzz.channels/channels"),
            ("buzz.me/me", "buzz.me/me"),
            ("settings", "settings"),
            ("example.header/main", "example.header/main"),
            ("missing/page", "missing/page"),
        ] {
            assert_eq!(
                area(Page::new(selected), &pages).key(),
                expected,
                "{selected}"
            );
        }
    }

    #[test]
    fn messages_is_never_borrowed_unless_it_is_a_header_page() {
        let inbox = Page::new("buzz.inbox/inbox");
        let others: Vec<PageSpec> = registered()
            .into_iter()
            .filter(|spec| spec.page != Page::MESSAGES)
            .collect();
        for messages in [None, Some(Placement::Sidebar), Some(Placement::Hidden)] {
            let mut pages = others.clone();
            pages.extend(messages.map(|placement| placed("buzz.channels/channels", placement)));
            assert_eq!(area(inbox, &pages), inbox);
        }
    }

    #[test]
    fn me_stays_separate_whatever_its_placement() {
        let mut pages: Vec<PageSpec> = registered()
            .into_iter()
            .filter(|spec| spec.page != Page::ME)
            .collect();
        pages.push(placed("buzz.me/me", Placement::Sidebar));
        assert_eq!(area(Page::ME, &pages), Page::ME);
    }

    #[test]
    fn other_pages_follow_by_label_then_key() {
        let pages = vec![
            (Page::new("x.b/page"), "beta"),
            (Page::new("x.a/page"), "Alpha"),
            (Page::new("x.c/page"), "Alpha"),
        ];
        assert_eq!(sorted(pages.clone()), ["Alpha", "Alpha", "beta"]);
        let mut keyed = pages;
        keyed.sort_by(|a, b| order(a.0, a.1, b.0, b.1));
        assert_eq!(keyed[0].0, Page::new("x.a/page"));
    }
}
