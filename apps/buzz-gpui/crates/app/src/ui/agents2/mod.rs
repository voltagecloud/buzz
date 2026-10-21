//! The Agents2 page and the views it shares with the profile panel
//! (buzz-app's `bundled/agents2` and `features/agents2/*.tsx`).

mod attention;
mod bring_over;
mod browse;
pub(crate) mod card;
mod create;
mod manage;
mod memory;
mod packs;
mod page;
pub(crate) mod routes;
mod tabs;
mod teams;

pub use attention::AttentionPanel;
pub(crate) use browse::model_icon;
pub use manage::{AgentDelete, Removed};
pub use page::Agents2Page;
pub(crate) use page::{agent_picture, agent_target};
pub use tabs::AgentTabs;

use super::pages::{self, Page, PageRoutes, PageSpec};
use crate::agents2::{AgentConversation, Agents2};
use buzz_ui::{ActiveTheme, Button, ButtonSize, Colors, IconName, TypeStyled, tokens::radius};
use gpui_kit::*;
use std::collections::HashMap;

pub const PAGE: Page = Page::AGENTS;

/// Registers the Agents2 primary page (`buzz.agents2`).
pub fn register(cx: &mut App) {
    pages::register(
        PageSpec {
            // Agents shares the channel sidebar and its collapse toggle.
            collapsible: true,
            ..PageSpec::sidebar(PAGE, "Agents", IconName::Robot, |window, cx| {
                cx.new(|cx| Agents2Page::new(window, cx)).into()
            })
        },
        cx,
    );
    pages::register_routes(
        PAGE,
        PageRoutes {
            version: 1,
            validate: routes::valid,
            open: |view, params, window, cx| {
                if let Ok(page) = view.clone().downcast::<Agents2Page>() {
                    page.update(cx, |page, cx| page.open_route(params, window, cx));
                }
            },
        },
        cx,
    );
}

/// `EmptyState`: a bordered card with an icon, a title, a description and
/// an optional action.
pub(crate) fn empty_state(
    icon: &'static str,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    action: Option<AnyElement>,
    c: &Colors,
) -> Div {
    div()
        .w_full()
        .min_w_0()
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::CONTAINER)
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(rems(1.))
                .py(rems(2.))
                .px(rems(1.5))
                .text_center()
                .child(div().text_heading().text_color(c.text_subtle).child(icon))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.5))
                        .max_w(rems(24.))
                        .child(div().text_label_sm().child(title.into()))
                        .child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child(description.into()),
                        ),
                )
                .children(action.map(|action| div().mt(rems(0.5)).child(action))),
        )
}

/// An error under a form or row (`role="alert"`).
pub(crate) fn problem(error: Option<&SharedString>, c: &Colors) -> Option<Div> {
    error.map(|error| {
        div()
            .text_body_sm()
            .text_color(c.text_danger)
            .child(error.clone())
    })
}

/// A compact button with a leading icon.
pub(crate) fn compact(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id, label).size(ButtonSize::Sm)
}

#[derive(Default)]
struct ProfileSections(HashMap<String, Entity<ProfileAgent>>);

impl Global for ProfileSections {}

/// What the profile panel adds for an Agents2 agent
/// (`ProfilePanel.tsx`): its type's tabs, Attention and Memory,
/// then "Delete
/// agent". `conversation` is the one the panel was opened from. `None`
/// for anyone else.
pub fn profile_sections(
    pubkey: &str,
    conversation: Option<AgentConversation>,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let service = Agents2::global(cx)?;
    service.read(cx).find(pubkey)?;
    let existing = cx
        .try_global::<ProfileSections>()
        .and_then(|sections| sections.0.get(pubkey).cloned());
    let view = match existing {
        Some(view) => view,
        None => {
            let view = cx.new(|cx| ProfileAgent::new(pubkey.to_owned(), window, cx));
            cx.default_global::<ProfileSections>()
                .0
                .insert(pubkey.to_owned(), view.clone());
            view
        }
    };
    // Views of agents that are gone are dropped.
    let live: Vec<String> = service
        .read(cx)
        .agents()
        .iter()
        .map(|a| a.pubkey.clone())
        .collect();
    cx.default_global::<ProfileSections>()
        .0
        .retain(|key, _| live.contains(key));
    let tabs = view.read(cx).tabs.clone();
    tabs.update(cx, |tabs, cx| tabs.set_conversation(conversation, cx));
    Some(view.into_any_element())
}

/// The profile panel's Agents2 part.
struct ProfileAgent {
    tabs: Entity<AgentTabs>,
    delete: Entity<AgentDelete>,
}

impl ProfileAgent {
    fn new(pubkey: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tabs = cx.new(|cx| AgentTabs::new(pubkey.clone(), window, cx));
        let delete = cx.new(|cx| AgentDelete::new(pubkey, cx));
        cx.observe(&tabs, |_, _, cx| cx.notify()).detach();
        Self { tabs, delete }
    }
}

impl Render for ProfileAgent {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.colors();
        div()
            .flex()
            .flex_col()
            .gap(rems(1.))
            .px(px(20.))
            .pb(px(20.))
            .border_t_1()
            .border_color(c.border_standard)
            .pt(rems(1.))
            .child(self.tabs.clone())
            .child(div().flex().child(self.delete.clone()))
    }
}
