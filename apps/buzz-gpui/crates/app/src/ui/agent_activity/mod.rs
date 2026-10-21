//! Agent activity views (buzz-app's bundled `agent-activity` plugin and
//! the shared activity rows): the Agent Activity panel, its transcript,
//! the composer's working indicator and "Working now" popover, the
//! sidebar's working popover and the profile's latest activity.

pub mod bubble;
pub mod panel;
pub mod profile;
pub mod row;
pub mod transcript;

use crate::agent_activity::{selection, target};
use crate::community::Community;
use crate::ui::channel_panels::{self, ChannelPanel};
use crate::ui::identity::AgentLibrary;
use crate::ui::markdown::Directory;
use buzz_ui::IconName;
use gpui_kit::*;
use nostr::nips::nip19::ToBech32;
use nostr::prelude::PublicKey;
use std::collections::HashMap;

/// The panel's key in the channel panel registry.
pub const PANEL: &str = "buzz.agent-activity/activity";

/// Registers the Agent Activity panel: it opens for
/// `buzz:agent-activity?…` targets as a tab beside the conversation.
pub fn init(cx: &mut App) {
    channel_panels::register(
        ChannelPanel {
            key: PANEL,
            order: 0,
            title: "Agent Activity",
            icon: IconName::ListBullets,
            matches: channel_panels::matches(|target| selection(target).is_some()),
            app_launcher: None,
            launcher: None,
            menu: None,
            build: channel_panels::build(|opening, window, cx| {
                cx.new(|cx| panel::ActivityPanel::new(opening, window, cx))
                    .into()
            }),
        },
        cx,
    );
    dev_open(cx);
}

/// Development only: `BUZZ_DEV_OPEN_ACTIVITY=<target>` opens that panel
/// once the first channel shows, so headless runs can screenshot it.
fn dev_open(cx: &mut App) {
    let Ok(target) = std::env::var("BUZZ_DEV_OPEN_ACTIVITY") else {
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(6))
            .await;
        cx.update(|cx| {
            for window in cx.windows() {
                let _ = window.update(cx, |_, window, cx| {
                    crate::ui::navigation::open_url(&target, window, cx)
                });
            }
        });
    })
    .detach();
}

/// Opens the agent's activity beside the open conversation.
pub fn open(
    agent: &str,
    channel: Option<&str>,
    thread: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    crate::ui::navigation::open_url(&target(agent, channel, thread), window, cx);
}

/// `npub…xyz`, with the shortest suffix that tells these keys apart
/// (`publicKeyLabels`).
pub fn public_key_labels<'a>(
    keys: impl IntoIterator<Item = &'a String>,
) -> HashMap<String, String> {
    let encoded: Vec<(String, String)> = keys
        .into_iter()
        .filter_map(|key| {
            let npub = PublicKey::from_hex(key).ok()?.to_bech32().ok()?;
            Some((key.to_ascii_lowercase(), npub))
        })
        .collect();
    let mut length = 3;
    let suffix = |npub: &str, length: usize| npub[npub.len() - length..].to_string();
    while length < 58 {
        let mut seen: Vec<String> = encoded.iter().map(|(_, n)| suffix(n, length)).collect();
        seen.sort();
        seen.dedup();
        if seen.len() == encoded.len() {
            break;
        }
        length += 1;
    }
    encoded
        .into_iter()
        .map(|(key, npub)| (key, format!("npub…{}", suffix(&npub, length))))
        .collect()
}

/// An agent's name in activity views: its signed or library name, else
/// `Agent · npub…xyz`.
pub fn agent_label(
    community: &Community,
    key: &str,
    keys: &HashMap<String, String>,
    cx: &App,
) -> String {
    let directory = Directory::of(community, None, cx);
    let fallback = community
        .snapshot
        .profiles
        .get(key)
        .filter(|profile| !profile.name_is_fallback)
        .map(|profile| profile.name.clone())
        .unwrap_or_else(|| {
            format!(
                "Agent · {}",
                keys.get(key).cloned().unwrap_or_else(|| key.to_string())
            )
        });
    directory.names.name(key, &fallback)
}

/// Who works in a conversation, for its working badge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Working {
    pub agents: Vec<String>,
    /// Every one of them is still only starting: received or handed
    /// over, with no step yet.
    pub waiting: bool,
}

/// The sidebar's working agents in `channel` (`useWorkingAgents`): the
/// viewer's agents typing there, unless this app tracks their work, and
/// agents with local work, observed working turns or typing there.
pub fn with_observed(
    typing: Vec<String>,
    community: &Entity<Community>,
    channel: &str,
    cx: &App,
) -> Working {
    let snapshot = crate::agent_activity::observer::snapshot(community, cx);
    let local = crate::agent_activity::local::local_activity(&snapshot.local, channel, None);
    let mut agents: Vec<String> = typing
        .into_iter()
        .filter(|agent| !local.contains_key(agent.as_str()))
        .collect();
    agents.extend(crate::agent_activity::current::working_agents(
        &snapshot, channel,
    ));
    agents.sort();
    agents.dedup();
    let waiting = !agents.is_empty()
        && agents.iter().all(|agent| {
            crate::agent_activity::local::breathing(local.get(agent.as_str()).copied())
        });
    Working { agents, waiting }
}

/// The viewer's agents in `community` (`agentChoices`).
pub fn owned_agents(community: &Community, cx: &App) -> std::collections::HashSet<String> {
    let library = AgentLibrary::from_hosts(cx);
    crate::agent_activity::current::owned_agents(
        library.agents.into_iter().map(|(key, _, _)| key),
        &community.snapshot.profiles,
        community.session.viewer(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn key_labels_grow_until_distinct() {
        let a = buzz_identity::Identity::ephemeral().pubkey_hex();
        let labels = public_key_labels([&a]);
        assert!(labels[&a].starts_with("npub…") && labels[&a].chars().count() == 8);
        assert!(public_key_labels([&"zz".to_string()]).is_empty());
    }
}
