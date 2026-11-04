//! Channel settings' owner-controlled Remote agent permissions.

use crate::community::Community;
use buzz_relay_client::agent_sharing::{ChannelAgent, ChannelAgents};
use buzz_ui::{
    ActiveTheme, Button, Spinner, Switch, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

pub(super) struct ChannelAgentSharing {
    community: Entity<Community>,
    channel: String,
    state: Option<ChannelAgents>,
    loading: bool,
    busy: bool,
    error: Option<String>,
    generation: u64,
}

impl ChannelAgentSharing {
    pub(super) fn new(
        community: Entity<Community>,
        channel: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            community,
            channel,
            state: None,
            loading: false,
            busy: false,
            error: None,
            generation: 0,
        };
        panel.reload(cx);
        panel
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        let (session, channel) = (
            self.community.read(cx).session.clone(),
            self.channel.clone(),
        );
        cx.spawn(async move |this, cx| {
            let result = session.channel_agent_sharing(&channel).await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(state) => this.state = Some(state),
                    Err(error) => {
                        this.state = None;
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn change(&mut self, agent: ChannelAgent, enabled: bool, cx: &mut Context<Self>) {
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.clone();
        self.run(cx, async move {
            session
                .set_channel_agent_sharing(&channel, &agent.pubkey, enabled)
                .await
        });
    }

    fn retry(&mut self, agent: ChannelAgent, cx: &mut Context<Self>) {
        let Some(pending) = agent.pending else {
            return;
        };
        let session = self.community.read(cx).session.clone();
        self.run(cx, async move {
            session.retry_channel_agent_sharing(pending.id).await
        });
    }

    fn discard(&mut self, agent: ChannelAgent, cx: &mut Context<Self>) {
        let Some(pending) = agent.pending else {
            return;
        };
        let session = self.community.read(cx).session.clone();
        self.run(cx, async move {
            session.discard_failed_agent_sharing(pending.id).await
        });
    }

    fn run(
        &mut self,
        cx: &mut Context<Self>,
        operation: impl Future<Output = Result<(), String>> + 'static,
    ) {
        if self.busy || self.loading {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.busy = true;
        self.error = None;
        let (session, channel) = (
            self.community.read(cx).session.clone(),
            self.channel.clone(),
        );
        cx.spawn(async move |this, cx| {
            let result = operation.await;
            let refreshed = session.channel_agent_sharing(&channel).await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.busy = false;
                this.error = result.err();
                match refreshed {
                    Ok(state) => this.state = Some(state),
                    Err(error) => {
                        this.state = None;
                        this.error.get_or_insert(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn row(&self, agent: &ChannelAgent, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let checked = agent.enabled;
        let disabled = self.loading
            || self.busy
            || agent.pending.is_some()
            || (!checked && agent.enable_reason.is_some());
        let weak = cx.entity().downgrade();
        let changed = agent.clone();
        let id: SharedString = format!("share-agent-{}", agent.pubkey).into();
        let toggle = gpui_kit::base::Switch::new(id.clone())
            .checked(checked)
            .disabled(disabled)
            .accessibility_label(format!("Allow channel members to use {}", agent.name))
            .child(Switch::new(id, checked).disabled(disabled))
            .focus_visible(move |style| style.border_2().border_color(c.border_focus))
            .on_change(move |enabled, _, _, cx| {
                let _ = weak.update(cx, |this, cx| this.change(changed.clone(), enabled, cx));
            });
        let status = if agent.pending.is_some() {
            "Change awaiting confirmation"
        } else if checked {
            "Shared with channel members"
        } else {
            "Only you can invoke this agent"
        };
        let mut row = div().flex().flex_col().gap(space::S2).py(space::S3).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(space::S3)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(space::S1)
                        .child(div().truncate().child(agent.name.clone()))
                        .child(div().text_body_sm().text_color(c.text_subtle).child(status)),
                )
                .child(toggle),
        );
        if let Some(reason) = &agent.enable_reason {
            row = row.child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(reason.clone()),
            );
        }
        if let Some(pending) = &agent.pending {
            if pending.enabled {
                let revoke = cx.entity().downgrade();
                let revoked = agent.clone();
                row = row.child(
                    action(
                        format!("revoke-share-{}", agent.pubkey),
                        "Revoke sharing",
                        self.busy || self.loading,
                        cx,
                    )
                    .accessibility_label(format!("Revoke pending sharing for {}", agent.name))
                    .on_click(move |_, _, cx| {
                        let _ =
                            revoke.update(cx, |this, cx| this.change(revoked.clone(), false, cx));
                    }),
                );
            }
            let retry = cx.entity().downgrade();
            let retried = agent.clone();
            row = row.child(
                div().flex().gap(space::S2).child(
                    action(
                        format!("retry-share-{}", agent.pubkey),
                        "Retry saved change",
                        self.busy || self.loading,
                        cx,
                    )
                    .accessibility_label(format!("Retry sharing change for {}", agent.name))
                    .on_click(move |_, _, cx| {
                        let _ = retry.update(cx, |this, cx| this.retry(retried.clone(), cx));
                    }),
                ),
            );
            if pending.failed {
                let discard = cx.entity().downgrade();
                let discarded = agent.clone();
                row = row.child(
                    action(
                        format!("discard-share-{}", agent.pubkey),
                        "Discard failed change",
                        self.busy || self.loading,
                        cx,
                    )
                    .accessibility_label(format!(
                        "Discard failed sharing change for {}",
                        agent.name
                    ))
                    .on_click(move |_, _, cx| {
                        let _ = discard.update(cx, |this, cx| this.discard(discarded.clone(), cx));
                    }),
                );
            }
            if let Some(error) = &pending.error {
                row = row.child(div().text_body_sm().child(error.clone()));
            }
        }
        row.into_any_element()
    }
}

fn action(
    id: impl Into<SharedString>,
    label: &str,
    disabled: bool,
    cx: &App,
) -> gpui_kit::base::Button {
    let id = id.into();
    let ring = cx.colors().border_focus;
    gpui_kit::base::Button::new(id.clone())
        .accessibility_label(label.to_owned())
        .disabled(disabled)
        .focus_visible(move |style| style.border_2().border_color(ring))
        .child(Button::new(id, label.to_owned()).disabled(disabled))
}

impl Render for ChannelAgentSharing {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let weak = cx.entity().downgrade();
        let agents = self
            .state
            .as_ref()
            .map(|s| s.agents.clone())
            .unwrap_or_default();
        div().id("channel-agent-sharing").flex().flex_col().gap(space::S3)
            .p(space::S4).border_1().border_color(c.border_standard).rounded(radius::PANEL)
            .child(div().flex().items_center().justify_between().child("Your Remote agents")
                .child(action("refresh-agent-sharing","Refresh",self.busy || self.loading,cx)
                    .on_click(move |_,_,cx| {let _=weak.update(cx,|this,cx| this.reload(cx));})))
            .child(div().text_body_sm().text_color(c.text_subtle).child("Let current channel members mention your agents and use their configured tools. You keep control of settings and recovery. Private DMs stay private."))
            .when(self.loading || self.busy,|panel| panel.child(Spinner::new().size(16.)))
            .when(self.state.is_some() && agents.is_empty(),|panel| panel.child("No Remote workspace agents to share. Move an agent to Remote workspace, then refresh."))
            .children(agents.iter().map(|agent| self.row(agent,cx)).collect::<Vec<_>>())
            .children(self.error.clone().map(|error| div().text_body_sm().child(error)))
    }
}
