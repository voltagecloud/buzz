//! Manual publication from the selected originating thread. No activity inputs.

use crate::community::Community;
use buzz_relay_client::Session;
use buzz_relay_client::session::team_status::{Preview, Status};
use buzz_ui::{ActiveTheme, Button, TextField, TypeStyled};
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

pub(super) struct TeamStatus {
    session: Arc<Session>,
    channel: String,
    root: String,
    agents: Vec<String>,
    selected: Option<String>,
    status: Status,
    caption: Entity<InputState>,
    preview: Option<Preview>,
    busy: bool,
    message: String,
}

impl TeamStatus {
    pub(super) fn new(
        community: Entity<Community>,
        channel: String,
        root: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut agents: Vec<_> = super::typing::owned_agents(cx).into_iter().collect();
        agents.sort();
        agents.truncate(128);
        Self::for_session(
            community.read(cx).session.clone(),
            channel,
            root,
            agents,
            window,
            cx,
        )
    }

    fn for_session(
        session: Arc<Session>,
        channel: String,
        root: String,
        agents: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            session,
            channel,
            root,
            selected: agents.first().cloned(),
            agents,
            status: Status::Working,
            caption: cx.new(|cx| {
                InputState::new(window, cx).placeholder("Optional owner-written public caption")
            }),
            preview: None,
            busy: false,
            message: String::new(),
        }
    }

    fn preview(&mut self, cx: &mut Context<Self>) {
        let Some(agent) = self.selected.clone() else {
            return;
        };
        let session = self.session.clone();
        let (channel, root, status, caption) = (
            self.channel.clone(),
            self.root.clone(),
            self.status,
            self.caption.read(cx).value().to_string(),
        );
        self.busy = true;
        self.preview = None;
        cx.spawn(async move |this, cx| {
            let result = session
                .preview_team_status(&agent, &channel, &root, status, &caption)
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(p) => {
                        this.preview = Some(p);
                        this.message.clear();
                    }
                    Err(e) => this.message = e,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn publish(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.preview.take() else {
            return;
        };
        let session = self.session.clone();
        self.busy = true;
        cx.spawn(async move |this, cx| {
            let result = session.share_team_status(preview, true).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.message = result
                    .err()
                    .unwrap_or_else(|| "Saved for publication. Check delivery below.".into());
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for TeamStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.clone();
        let state = session.team_status_state();
        let c = *cx.colors();
        let frozen = self.busy || self.preview.is_some();
        let agents = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let agent = agent.clone();
                action(
                    ("status-agent", i),
                    format!(
                        "Agent {}{}",
                        &agent[..12],
                        if self.selected.as_ref() == Some(&agent) {
                            " (selected)"
                        } else {
                            ""
                        }
                    ),
                    frozen,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = Some(agent.clone());
                    cx.notify();
                }))
            })
            .collect::<Vec<_>>();
        let statuses = [
            Status::Working,
            Status::Waiting,
            Status::Blocked,
            Status::Done,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, status)| {
            action(
                ("team-status-enum", i),
                format!(
                    "{}{}",
                    status.label(),
                    if self.status == status {
                        " (selected)"
                    } else {
                        ""
                    }
                ),
                frozen,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.status = status;
                cx.notify();
            }))
        })
        .collect::<Vec<_>>();
        div().id("team-status-controls").max_h(px(360.)).overflow_y_scroll().flex_none().flex().flex_col().gap(px(8.)).p(px(12.)).text_body_sm().text_color(c.text_standard)
            .child("Share coarse team status — off by default")
            .child("Only run one owner publisher across your devices. All authorized channel readers, including future members, can read this status. Received information cannot be recalled.")
            .children(agents)
            .child(div().flex().flex_wrap().gap(px(4.)).children(statuses))
            .child(TextField::new(&self.caption).disabled(frozen))
            .child(action("preview-team-status", "Preview exact publication", frozen || self.selected.is_none(), cx)
                .on_click(cx.listener(|this, _, _, cx| this.preview(cx))))
            .children(self.preview.as_ref().map(|p| {
                let scope = p.scope();
                div().flex().flex_col().gap(px(4.))
                    .child(format!("Label: {}", p.label()))
                    .child(format!("Agent: {}", scope.agent))
                    .child(format!("Community: {}", scope.community))
                    .child(format!("Channel: {}", scope.channel))
                    .child(format!("Root: {}", scope.root))
                    .child("Fresh for 15 minutes. This is an owner assertion, not proof of completion.")
                    .child(action("confirm-shareable-status", "I confirm shareable work here and one owner publisher; publish", self.busy, cx).on_click(cx.listener(|this, _, _, cx| this.publish(cx))))
                    .child(action("cancel-team-preview", "Cancel preview", self.busy, cx)
                        .on_click(cx.listener(|this, _, _, cx| { this.preview = None; cx.notify(); })))
            }))
            // Always available, including during a publication or failed preflight.
            .child(action("stop-team-status", "Stop all team status sharing", false, cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.preview = None;
                    this.message = this.session.stop_team_status().err()
                        .unwrap_or_else(|| "Sharing stopped. Saved clears retry when their originating communities are connected.".into());
                    cx.notify();
                })))
            .child(format!("{} enabled; {} pending writes or clears", state.sharing, state.pending))
            .children(state.error)
            .child(self.message.clone())
            .child(action("refresh-team-status", "Refresh delivery status", false, cx)
                .on_click(cx.listener(|_, _, _, cx| cx.notify())))
    }
}

// buzz_ui::Button supplies visuals; the base control owns keyboard and semantics.
pub(super) fn action(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    disabled: bool,
    cx: &App,
) -> gpui_kit::base::Button {
    let id = id.into();
    let label = label.into();
    let ring = cx.colors().border_focus;
    gpui_kit::base::Button::new(id.clone())
        .accessibility_label(label.clone())
        .disabled(disabled)
        .focus_visible(move |style| style.border_2().border_color(ring))
        .child(Button::new(id, label).disabled(disabled))
}

#[cfg(test)]
mod tests;
