//! Workflows (`src/bundled/workflows`, `buzz.workflows`): channel-scoped
//! YAML configurations the relay runs. The page lists them per channel as
//! cards and edits them in a form or as YAML; the session owns reads and
//! commands (`buzz_relay_client::workflows`).

mod channel;
mod controls;
mod editor;
mod landing;
mod model;
mod operations;
mod page;
mod runs;
mod secrets;

use crate::community::{AppState, Community};
use crate::ui::pages::{self, Page, PageSpec};
use buzz_relay_client::SessionStatus;
use buzz_ui::{ActiveTheme, Button, IconName, TypeStyled, tokens::space};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use page::WorkflowCommunity;

pub const PAGE: Page = Page::new("buzz.workflows/workflows");

/// Compact surfaces still identify automation, never the owner as the
/// sender (`workflowLabel`).
pub fn workflow_label(owner_name: &str) -> String {
    format!("Workflow · owned by {owner_name}")
}

/// Who a row reads as from: its author, or for relay-attested workflow
/// output "Workflow · owned by" its owner (`senderName` in
/// `ThreadPanel.tsx`).
pub fn sender_name(
    profiles: &buzz_relay_client::Profiles,
    row: &buzz_relay_client::MessageRow,
) -> SharedString {
    match row.workflow_owner {
        Some(owner) => {
            workflow_label(&crate::ui::message::display_name(profiles, &owner.to_hex())).into()
        }
        None => crate::ui::message::display_name(profiles, &row.author.to_hex()),
    }
}

pub fn register(cx: &mut App) {
    pages::register(
        PageSpec::sidebar(PAGE, "Workflows", IconName::Lightning, |_, cx| {
            cx.new(WorkflowsPage::new).into()
        }),
        cx,
    );
}

/// The page (`WorkflowsPage`): the selected community's workflows once its
/// relay is ready, else why not.
pub struct WorkflowsPage {
    community: Option<(Entity<Community>, Entity<WorkflowCommunity>)>,
    /// The relay was ready at the last look; a new readiness starts a
    /// fresh view, as buzz-app keys it by the connection's generation.
    ready: bool,
    _observers: Vec<Subscription>,
}

impl WorkflowsPage {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            community: None,
            ready: false,
            _observers: vec![cx.observe_global::<AppState>(|this, cx| this.follow(cx))],
        };
        this.follow(cx);
        this
    }

    /// Keeps one community view per selected community.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let current = AppState::current(cx);
        if current.as_ref().map(Entity::entity_id)
            != self.community.as_ref().map(|(c, _)| c.entity_id())
        {
            self.community = None;
            self._observers.truncate(1);
            if let Some(community) = current {
                self._observers
                    .push(cx.observe(&community, |this, community, cx| {
                        let ready = community.read(cx).snapshot.status == SessionStatus::Ready;
                        if ready && !this.ready {
                            let view = cx.new(|cx| WorkflowCommunity::new(community.clone(), cx));
                            this.community = Some((community, view));
                        }
                        this.ready = ready;
                        cx.notify();
                    }));
                self.ready = community.read(cx).snapshot.status == SessionStatus::Ready;
                let view = cx.new(|cx| WorkflowCommunity::new(community.clone(), cx));
                self.community = Some((community, view));
            }
        }
        cx.notify();
    }
}

impl Render for WorkflowsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        if let Some((community, view)) = &self.community {
            let status = community.read(cx).snapshot.status.clone();
            if status == SessionStatus::Ready {
                return div().size_full().child(view.clone()).into_any_element();
            }
            return state(Some(status), community.clone(), cx).into_any_element();
        }
        let _ = c;
        state_without_community(cx).into_any_element()
    }
}

fn header(cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .child(div().text_title().child("Workflows"))
        .child(
            div()
                .mt(space::S2)
                .text_body()
                .text_color(cx.colors().text_subtle)
                .child("Automations that keep your community moving."),
        )
}

fn frame(content: Div, cx: &App) -> Div {
    div()
        .size_full()
        .p(space::S6)
        .text_body()
        .text_color(cx.colors().text_standard)
        .child(
            div()
                .flex()
                .flex_col()
                .w_full()
                .max_w(rems(72.))
                .mx_auto()
                .child(header(cx))
                .child(content),
        )
}

fn state(status: Option<SessionStatus>, community: Entity<Community>, cx: &App) -> Div {
    let connecting = matches!(status, Some(SessionStatus::Connecting));
    let error = match status {
        Some(SessionStatus::Failed(error)) => Some(error),
        _ => None,
    };
    frame(
        div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .py(space::S8)
            .child(div().child(if connecting {
                "Connecting to your community…"
            } else {
                "Connect to a community to browse workflows."
            }))
            .when_some(error, |this, error| {
                this.child(div().child(error)).child(
                    div().flex().child(
                        Button::new("workflows-retry", "Retry connection")
                            .on_click(move |_, _, cx| community.read(cx).session.retry()),
                    ),
                )
            }),
        cx,
    )
}

fn state_without_community(cx: &App) -> Div {
    frame(
        div()
            .py(space::S8)
            .child("Connect to a community to browse workflows."),
        cx,
    )
}
