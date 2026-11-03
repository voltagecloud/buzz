//! Remote workspace agents: each thread the agent is mentioned in gets its
//! own Kubernetes Sandbox, which the agent's host asks Voltage's Sandbox
//! manager to start over the relay (`buzz-backend-relay`). The harness runs in the Sandbox and
//! talks to the community itself; this type only starts the session.
//! Built only with the `voltage-sandbox` feature.

mod codex_login;
pub mod config;
mod enroll;
mod enroll_form;
mod recovery;
mod settings;
mod signing;

use crate::agent_activity::local::WorkOutcome;
use crate::agents2::runtime::HostLink;
use crate::agents2::{AgentTab, AgentType, Agents2, Delivery, Trigger, TypeDefaults};
use buzz_host::api::{
    SandboxRequest, SandboxResponse, SandboxScope, SandboxSession, SandboxTenant,
};
use buzz_remote_sessions::SessionError;
use config::Config;
use gpui_kit::*;
use std::sync::Arc;
use std::time::Duration;

/// Registered as `voltage.remote-workspace/sandbox`.
pub const PLUGIN: &str = "voltage.remote-workspace";
pub const TYPE_ID: &str = "sandbox";

/// Over the host's 10 minutes to start a session, under the 30-minute
/// limit.
const START: Duration = Duration::from_secs(11 * 60);

/// Registers the type. Call after `agents2::init`.
pub fn init(cx: &mut App) {
    if let Err(error) = crate::agents2::register_type(PLUGIN, Arc::new(RemoteWorkspace), cx) {
        tracing::error!(%error, "could not register the remote workspace agent type");
        return;
    }
    recovery::init(cx);
}

struct RemoteWorkspace;

/// Shared destination editor for a batch; per-agent instructions and
/// environments are deliberately absent from this form.
pub(crate) fn migration_settings(
    view: crate::agents2::AgentView,
    window: &mut Window,
    cx: &mut App,
) -> AnyView {
    cx.new(|cx| settings::SettingsTab::shared(view, window, cx))
        .into()
}

impl AgentType for RemoteWorkspace {
    fn id(&self) -> &'static str {
        TYPE_ID
    }

    fn title(&self) -> SharedString {
        "Remote workspace".into()
    }

    fn description(&self) -> Option<SharedString> {
        Some(
            "Each channel thread or direct-message conversation runs in its own Kubernetes Sandbox."
                .into(),
        )
    }

    fn defaults(&self) -> TypeDefaults {
        TypeDefaults {
            config: Config::default().value(),
            attention: Default::default(),
        }
    }

    fn create_instructions(&self) -> bool {
        true
    }

    /// The instructions become the Sandbox harness's system prompt. A local
    /// workspace has no meaning in a Sandbox, so it stays behind.
    fn adopt(&self, carry: &buzz_host::api::Carry, saved: serde_json::Value) -> serde_json::Value {
        let mut config = Config::read(&saved);
        if let Some(instructions) = &carry.instructions {
            config.instructions = instructions.clone();
        }
        config.value()
    }

    fn carry(&self, saved: &serde_json::Value) -> buzz_host::api::Carry {
        buzz_host::api::Carry {
            instructions: Some(Config::read(saved).instructions)
                .filter(|text| !text.trim().is_empty()),
            workspace: None,
        }
    }

    fn summary(&self, agent: &crate::agents2::Agent) -> Option<String> {
        let config = Config::read(&agent.config);
        let text = |name: &str| config.provider_config.get(name)?.as_str();
        let relay = url::Url::parse(text("relay_url")?).ok()?;
        Some(format!("Sandbox manager · {}", relay.host_str()?))
    }

    fn tabs(&self) -> Vec<AgentTab> {
        vec![AgentTab {
            id: "settings".into(),
            title: "Settings".into(),
            build: Arc::new(|view, window, cx| {
                cx.new(|cx| settings::SettingsTab::new(view, window, cx))
                    .into()
            }),
        }]
    }

    fn runs(&self) -> bool {
        true
    }

    fn run(&self, delivery: Delivery, cx: &mut App) -> Task<Result<(), String>> {
        let route = routing(&delivery.trigger, || {
            let agents = Agents2::global(cx)?;
            let agents = agents.read(cx);
            let agent = agents
                .running()
                .iter()
                .find(|agent| agent.pubkey == delivery.agent.pubkey)?;
            let community = agents.relay(&agent.pubkey)?;
            let channel_type = delivery.channel.as_deref().and_then(|channel| {
                community
                    .read(cx)
                    .snapshot
                    .channels
                    .iter()
                    .find(|item| item.id == channel)
                    .and_then(|item| item.channel_type)
            });
            Some((agent.relay.clone(), channel_type))
        });
        let (relay, direct_message) = match route {
            Ok(Some(route)) => route,
            Ok(None) => return Task::ready(Ok(())),
            Err(error) => return Task::ready(Err(error)),
        };
        let name = delivery.agent.name.clone();
        let started =
            crate::runtime::Runtime::handle(cx).spawn(deliver(delivery, relay, direct_message));
        cx.spawn(async move |cx| {
            let sent = started
                .await
                .map_err(|_| "The start was cancelled".to_string())??;
            let Some(sent) = sent else {
                return Ok(());
            };
            cx.update(|cx| recovery::started(&sent, &name, cx));
            sent.result.map_err(|error| error.message)
        })
    }

    fn timeout(&self) -> Duration {
        START
    }
}

/// Ignore autonomous triggers before lookup; unknown channel metadata keeps
/// the existing channel-thread route for explicit mentions.
fn routing(
    trigger: &Trigger,
    lookup: impl FnOnce() -> Option<(String, Option<buzz_relay_client::ChannelType>)>,
) -> Result<Option<(String, bool)>, String> {
    if !matches!(trigger, Trigger::Mention { .. }) {
        return Ok(None);
    }
    lookup()
        .map(|(relay, kind)| Some((relay, kind == Some(buzz_relay_client::ChannelType::Dm))))
        .ok_or_else(|| "The agent or its community is no longer available.".into())
}

/// The session a mention starts, and how its start ended. Watches, timers
/// and mentions from anyone but the owner start nothing; a mention that
/// cannot start one fails before anything is sent.
pub(crate) async fn deliver(
    delivery: Delivery,
    relay: String,
    direct_message: bool,
) -> Result<Option<Sent>, String> {
    let Some(start) = start_of(&delivery, relay, direct_message) else {
        return Ok(None);
    };
    let (session, replay_floor) = start?;
    // Shown as working, and typing in the thread, until the host answers.
    let (sent, outcome) = futures::channel::oneshot::channel();
    delivery
        .working
        .until(async move { outcome.await.unwrap_or_else(|_| WorkOutcome::ok()) });
    let request = SandboxRequest::Start {
        session: session.clone(),
        replay_floor: Some(replay_floor),
    };
    let result = send(&delivery.agent.host, request).await.map(drop);
    let _ = sent.send(match &result {
        Ok(()) => WorkOutcome::ok(),
        Err(error) => WorkOutcome::failed(error.message.clone()),
    });
    Ok(Some(Sent {
        session,
        result,
        replay_floor,
    }))
}

/// A session request the host answered.
#[derive(Debug)]
pub(crate) struct Sent {
    pub session: SandboxSession,
    pub result: Result<(), SessionError>,
    /// The signed mention that requested this start, retained for recovery.
    pub replay_floor: u64,
}

/// The mention's thread session and the time the harness replays from,
/// for a mention by the owner or an agent it attested.
fn start_of(
    delivery: &Delivery,
    relay: String,
    direct_message: bool,
) -> Option<Result<(SandboxSession, u64), String>> {
    let Trigger::Mention { event } = &delivery.trigger else {
        return None;
    };
    if !super::from_owner(&super::event_of(event), &delivery.agent.owner) {
        return None;
    }
    let Some(channel) = delivery.channel.clone() else {
        return Some(Err("The mention is not in a channel".into()));
    };
    let config = Config::read(&delivery.config);
    if let Err(error) = config.validate() {
        return Some(Err(error));
    }
    let agent = &delivery.agent;
    let session = SandboxSession {
        pubkey: agent.pubkey.clone(),
        tenant: SandboxTenant {
            relay,
            signer: agent.owner.clone(),
        },
        scope: if direct_message {
            SandboxScope::DirectMessage {
                dm_channel_id: channel,
            }
        } else {
            SandboxScope::Thread {
                channel_id: channel,
                thread_root: crate::agents2::working::thread_root(event),
            }
        },
        spec: config.spec(&agent.name, delivery.team.as_deref()),
    };
    // The harness replays from just before the mention, so it sees it.
    Some(Ok((session, event.created_at)))
}

/// Sends one session request; a refusal keeps its code.
pub(crate) async fn send(
    host: &HostLink,
    request: SandboxRequest,
) -> Result<SandboxResponse, SessionError> {
    match host.sandbox(request).await {
        Ok(SandboxResponse::Failed { message, code }) => Err(SessionError { message, code }),
        Ok(response) => Ok(response),
        Err(message) => Err(message.into()),
    }
}

#[cfg(test)]
mod tests;
