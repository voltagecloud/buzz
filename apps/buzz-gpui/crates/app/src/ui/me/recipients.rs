//! Who a session message is for (`sessions/recipients.ts`) and which
//! agents a session may admit (`agents/choices.ts` `selectable`).
//!
//! A message in a session with one agent goes to that agent without a
//! mention; with several, the sender must @mention who should respond.
//! Classification never grants access: candidates come from the relay's
//! roster.

use super::super::identity::AgentLibrary;
use crate::community::Community;
use buzz_relay_client::session::AgentRules;
use buzz_relay_client::{ChannelSummary, ChannelType, Profiles, RelayOrigin};
use gpui_kit::{App, Entity};
use std::collections::HashSet;

/// The viewer's agents in this community, as a session may admit them
/// (`selectable`): the agents its hosts run for this relay, less those
/// the archive hides (`archived`).
pub fn rules(community: &Entity<Community>, cx: &App) -> AgentRules {
    let origin = community.read(cx).origin.clone();
    let library = AgentLibrary::from_hosts(cx);
    // An archived agent is not offered, and refused (`sessionAgentRules`).
    let archived = crate::ui::archives::hidden(community, cx);
    let mut seen = HashSet::new();
    let mut selectable: Vec<String> = library
        .agents
        .into_iter()
        .filter(|(_, _, relay)| RelayOrigin::parse(relay).is_ok_and(|other| other == origin))
        .filter(|(key, _, _)| !archived.contains(key) && seen.insert(key.clone()))
        .map(|(key, _, _)| key)
        .collect();
    // Your agents listed in this community from other devices too, while
    // that list is read (`agentChoices` `selectable`).
    if let Some(agents) = crate::agents2::Agents2::global(cx)
        && agents.read(cx).listing_state(&origin.http())
            == crate::agents2::listings::ListingRead::Ready
    {
        for (pubkey, _) in agents.read(cx).listed_elsewhere(&origin.http()) {
            if !archived.contains(&pubkey) && seen.insert(pubkey.clone()) {
                selectable.push(pubkey);
            }
        }
    }
    AgentRules {
        selectable,
        archived,
    }
}

/// The session's agents, or `None` while that cannot be told yet
/// (`sessionAgents`): missing profiles could hide a second agent, so no
/// sole recipient is guessed.
pub fn session_agents(
    channel: &ChannelSummary,
    profiles: &Profiles,
    known: &[String],
    viewer: &str,
) -> Option<Vec<String>> {
    if channel.members.is_empty() {
        return None;
    }
    let mut members: Vec<&String> = channel
        .members
        .iter()
        .filter(|key| *key != viewer)
        .collect();
    members.dedup();
    if members
        .iter()
        .any(|key| !known.contains(key) && !profiles.contains_key(key.as_str()))
    {
        return None;
    }
    Some(
        members
            .into_iter()
            .filter(|key| {
                known.contains(key) || profiles.get(key.as_str()).is_some_and(|p| p.is_agent)
            })
            .cloned()
            .collect(),
    )
}

/// The recipients of a message in `channel` (`sessionRecipients`): the
/// explicit mentions, or in a session without any, its only agent.
pub fn session_recipients(
    channel: &ChannelSummary,
    profiles: &Profiles,
    known: &[String],
    viewer: &str,
    explicit: Vec<String>,
) -> Result<Vec<String>, String> {
    if channel.channel_type != Some(ChannelType::Session) || !explicit.is_empty() {
        return Ok(explicit);
    }
    let agents = session_agents(channel, profiles, known, viewer)
        .ok_or("Session participants are still loading. Retry or @mention an agent.".to_owned())?;
    if agents.len() > 1 {
        return Err(
            "There are multiple agents in this session. @mention who should respond.".into(),
        );
    }
    Ok(agents)
}

/// A selected assistant that left the viewer's agents (`prepareRecipients`).
pub const ASSISTANT_UNAVAILABLE: &str =
    "The selected assistant is no longer available. Choose another assistant and retry.";

/// Me's recipients for a send: `selected` (the chat's assistant, given
/// only when the message mentions nobody) if still one of the viewer's
/// agents, else the explicit mentions.
pub fn with_selected(
    explicit: Vec<String>,
    selected: Option<String>,
    rules: &AgentRules,
) -> Result<Vec<String>, String> {
    match selected {
        Some(agent) if !rules.selectable.contains(&agent) => Err(ASSISTANT_UNAVAILABLE.into()),
        Some(agent) => Ok(vec![agent]),
        None => Ok(explicit),
    }
}

/// Before a send in a session: the recipients, with any mentioned agent
/// who is not a member added first (`prepareRecipients`). In Me only the
/// viewer's own agents may be added, and a message naming nobody goes to
/// the `selected` assistant.
#[allow(clippy::too_many_arguments)]
pub async fn prepare(
    session: &buzz_relay_client::Session,
    channel: &str,
    explicit: Vec<String>,
    selected: Option<String>,
    personal: bool,
    known: Vec<String>,
    profiles: Profiles,
    rules: AgentRules,
) -> Result<Vec<String>, String> {
    let summary = session
        .channels_now()
        .iter()
        .find(|c| c.id == channel)
        .cloned()
        .ok_or("The session changed. Review its channel and retry.")?;
    let recipients = if personal {
        with_selected(explicit, selected, &rules)?
    } else {
        session_recipients(&summary, &profiles, &known, session.viewer(), explicit)?
    };
    let missing: Vec<String> = recipients
        .iter()
        .filter(|key| !summary.members.contains(key))
        .cloned()
        .collect();
    if !missing.is_empty() {
        if personal && missing.iter().any(|key| !rules.selectable.contains(key)) {
            return Err("Only your available agents can be mentioned in Me.".into());
        }
        session.add_agents(channel, &missing, &rules).await?;
    }
    Ok(recipients)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn session(members: &[&str]) -> ChannelSummary {
        ChannelSummary {
            id: "11111111-1111-4111-8111-111111111111".into(),
            channel_type: Some(ChannelType::Session),
            members: members.iter().map(|m| (*m).into()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn one_agent_is_the_default_recipient() {
        let known = vec!["agent".to_owned()];
        let profiles = Profiles::default();
        let channel = session(&["me", "agent"]);
        assert_eq!(
            session_recipients(&channel, &profiles, &known, "me", Vec::new()).unwrap(),
            ["agent"]
        );
        // Explicit mentions win.
        assert_eq!(
            session_recipients(&channel, &profiles, &known, "me", vec!["x".into()]).unwrap(),
            ["x"]
        );
    }

    #[test]
    fn a_selected_assistant_takes_plain_sends_while_available() {
        let rules = AgentRules {
            selectable: vec!["a".into()],
            archived: Vec::new(),
        };
        assert_eq!(
            with_selected(Vec::new(), Some("a".into()), &rules).unwrap(),
            ["a"]
        );
        assert_eq!(
            with_selected(vec!["b".into()], None, &rules).unwrap(),
            ["b"]
        );
        assert!(with_selected(Vec::new(), None, &rules).unwrap().is_empty());
        assert_eq!(
            with_selected(Vec::new(), Some("gone".into()), &rules).unwrap_err(),
            ASSISTANT_UNAVAILABLE
        );
    }

    #[test]
    fn several_or_unknown_members_need_a_mention() {
        let known = vec!["a".to_owned(), "b".to_owned()];
        let profiles = Profiles::default();
        assert_eq!(
            session_recipients(
                &session(&["me", "a", "b"]),
                &profiles,
                &known,
                "me",
                Vec::new()
            )
            .unwrap_err(),
            "There are multiple agents in this session. @mention who should respond."
        );
        assert_eq!(
            session_recipients(
                &session(&["me", "a", "z"]),
                &profiles,
                &known,
                "me",
                Vec::new()
            )
            .unwrap_err(),
            "Session participants are still loading. Retry or @mention an agent."
        );
        // Not a session: nobody is guessed.
        let stream = ChannelSummary {
            channel_type: Some(ChannelType::Stream),
            ..session(&["me", "a"])
        };
        assert!(
            session_recipients(&stream, &profiles, &known, "me", Vec::new())
                .unwrap()
                .is_empty()
        );
    }
}
