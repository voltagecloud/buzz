//! Development only: `BUZZ_DEV_CLAUDE_CODE_TURNS=1` hands the selected
//! community's first Claude agent two mentions from its owner, so its
//! Running sessions list fills without live events (the TLS test relay's
//! WebSocket is not trusted by the relay client, so none arrive there).
//! Debug builds only.

use super::{PLUGIN, TYPE_ID};
use crate::agents2::{Agents2, Status};
use gpui_kit::*;

pub fn init(cx: &mut App) {
    if std::env::var_os("BUZZ_DEV_CLAUDE_CODE_TURNS").is_none_or(|value| value.is_empty()) {
        return;
    }
    let key = format!("{PLUGIN}/{TYPE_ID}");
    cx.spawn(async move |cx| {
        for _ in 0..240 {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let started = cx.update(|cx| {
                let agents = Agents2::global(cx)?;
                let read = agents.read(cx);
                if *read.status() != Status::Ready {
                    return None;
                }
                let pubkey = read
                    .agents()
                    .iter()
                    .find(|agent| agent.agent_type == key)?
                    .pubkey
                    .clone();
                turns(&pubkey, cx);
                Some(())
            });
            if started.is_some() {
                return;
            }
        }
        tracing::warn!("no Claude agent to hand turns to");
    })
    .detach();
}

/// `BUZZ_DEV_CLAUDE_CODE_TURNS`: hands the agent two mentions from its
/// owner in the selected channel, for the Running sessions list, without
/// waiting on live events.
fn turns(pubkey: &str, cx: &mut App) {
    use super::runtime::{Run, Wake};
    use crate::agents2::AgentHandle;
    use crate::agents2::runtime::HostLink;
    use crate::buzz_mcp::BuzzEvent;
    let Some(agents) = Agents2::global(cx) else {
        return;
    };
    let (Some(agent), Some(host)) = (
        agents.read(cx).find(pubkey).cloned(),
        agents.read(cx).host_of(pubkey),
    ) else {
        return;
    };
    let Some(state) = super::ClaudeCode::global(cx) else {
        return;
    };
    let channel = crate::community::AppState::current(cx)
        .and_then(|community| community.read(cx).remembered_channel(cx))
        .unwrap_or_default();
    let handle = AgentHandle {
        pubkey: agent.pubkey.clone(),
        name: agent.name.clone(),
        owner: agent.owner.clone(),
        host: HostLink {
            client: host.read(cx).client(),
            runtime: crate::runtime::Runtime::handle(cx),
        },
        wrote: Default::default(),
        attention: crate::agents2::Agents2::global(cx)
            .map(|agents| agents.read(cx).attention(&agent.pubkey))
            .unwrap_or_else(|| crate::agents2::runtime::AgentAttention::detached(&agent.pubkey)),
    };
    let spawn = host.read(cx).spawner(super::PROCESS_OWNER);
    let runtime = state.read(cx).runtime.clone();
    let config = agent.config.clone();
    crate::runtime::Runtime::handle(cx).spawn(async move {
        for content in [
            "Claude, what changed in the build?",
            "Claude, please look at the flaky test",
        ] {
            let event = BuzzEvent {
                id: format!("{:064x}", uuid::Uuid::new_v4().as_u128()),
                pubkey: handle.owner.clone(),
                created_at: chrono::Utc::now().timestamp() as u64,
                kind: 9,
                content: content.into(),
                tags: vec![vec!["h".into(), channel.clone()]],
                sig: String::new(),
            };
            runtime
                .run(Run {
                    wake: Wake::Mention { event },
                    channel: Some(channel.clone()),
                    agent: std::sync::Arc::new(handle.clone()),
                    config: config.clone(),
                    spawn: spawn.clone(),
                    model: None,
                    effort: None,
                    current: crate::agents2::runtime::always_current(),
                    team: String::new(),
                    working: std::sync::Arc::new(|_| {}),
                    activity: None,
                })
                .await
                .ok();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}
