//! `@` completion (`MentionCompletion.tsx`, `use-mention-choices.ts`,
//! `useMentionDirectory.ts`): the channel's members, the viewer's agents
//! and people a community directory search finds, labelled by the
//! contextual name policy and ordered by the mention rules.

use super::mention_rules::{self, Choice, Presence};
use crate::ui::identity::{self, AgentLibrary};
use buzz_relay_client::{ChannelSummary, ChannelType, PresenceState, Profiles};
use std::collections::{HashMap, HashSet, VecDeque};

/// Rows shown at most ("Narrow your search to see more members.").
pub const LIMIT: usize = 50;

/// A person row.
#[derive(Clone, Debug, PartialEq)]
pub struct Person {
    pub pubkey: String,
    /// The identity's own name, which a pick inserts.
    pub name: String,
    /// The label shown, with any namesake qualifier.
    pub label: String,
    pub picture: Option<String>,
    pub agent: bool,
    pub member: bool,
}

/// The directory search for the current query.
pub enum Directory<'a> {
    /// No search applies (an empty query, or a session).
    Idle,
    /// Waiting or running; the last answer's people still show.
    Loading(&'a [String]),
    Done {
        people: &'a [String],
        more: bool,
    },
    Failed,
}

/// What the popup shows.
pub struct Completion {
    pub people: Vec<Person>,
    pub status: Option<&'static str>,
    pub retry: bool,
    /// The one person Space selects, when the choice set is complete.
    pub space: Option<String>,
    /// A multi-word query that continues no known name is prose.
    pub closed: bool,
}

pub struct Inputs<'a> {
    pub channel: Option<&'a ChannelSummary>,
    pub roster_ready: bool,
    pub profiles: &'a Profiles,
    pub library: &'a AgentLibrary,
    pub viewer: &'a str,
    pub origin: Option<&'a buzz_relay_client::RelayOrigin>,
    pub presence: &'a HashMap<String, PresenceState>,
    pub history: &'a HashMap<String, u64>,
    /// Saved team names, which also make a query a search.
    pub teams: &'a [String],
    /// The session can write member additions (`outbox.supports(9000)`).
    pub can_invite: bool,
}

/// Whether people outside the channel can be found: not in sessions.
pub fn searches_directory(channel: Option<&ChannelSummary>) -> bool {
    channel.is_none_or(|c| c.channel_type != Some(ChannelType::Session))
}

/// Whether the viewer's own agents are offered beyond the members
/// (`mentionCandidates`): all of them in a session, which invites them;
/// in a stream or forum only when the viewer can add members, since a
/// send asks to add them; never in a DM or elsewhere.
pub fn offers_agents(channel: Option<&ChannelSummary>, can_invite: bool) -> bool {
    match channel.and_then(|c| c.channel_type) {
        Some(ChannelType::Session) => true,
        Some(ChannelType::Stream | ChannelType::Forum) => can_invite,
        _ => false,
    }
}

/// The `@query` popup (`MentionCompletion`).
pub fn complete(inputs: &Inputs, query: &str, directory: Directory) -> Completion {
    let channel = inputs.channel;
    // Archived or read-only channels have no choices.
    if channel.is_some_and(|c| c.archived || (c.read_only && !c.cached)) {
        return Completion {
            people: Vec::new(),
            status: None,
            retry: false,
            space: None,
            closed: true,
        };
    }
    let members: Vec<String> = channel.map(|c| c.members.clone()).unwrap_or_default();
    let member_set: HashSet<&str> = members.iter().map(String::as_str).collect();
    let mut keys: Vec<String> = members.clone();
    let mut seen: HashSet<String> = keys.iter().cloned().collect();
    if offers_agents(channel, inputs.can_invite) {
        for (key, _, _) in &inputs.library.agents {
            if seen.insert(key.clone()) {
                keys.push(key.clone());
            }
        }
    }
    // Keys known without the directory search.
    let local: HashSet<String> = keys.iter().cloned().collect();
    let (found, searching, failed, more) = match directory {
        Directory::Idle => (&[][..], false, false, false),
        Directory::Loading(people) => (people, true, false, false),
        Directory::Done { people, more } => (people, false, false, more),
        Directory::Failed => (&[][..], false, true, false),
    };
    for key in found {
        if seen.insert(key.clone()) {
            keys.push(key.clone());
        }
    }
    // Labels are resolved against the whole choice set.
    let names = identity::names(
        inputs.profiles,
        inputs.library,
        inputs.viewer,
        inputs.origin,
        Some(&keys),
    );
    let library: HashMap<&str, &str> = inputs
        .library
        .agents
        .iter()
        .map(|(key, name, _)| (key.as_str(), name.as_str()))
        .collect();
    let choices: Vec<Choice> = keys
        .iter()
        .filter(|key| key.len() == 64)
        .map(|key| {
            let profile = inputs.profiles.get(key.as_str());
            let mut aliases = Vec::new();
            if let Some(profile) =
                profile.filter(|p| !p.name_is_fallback && !p.name.trim().is_empty())
            {
                aliases.push(profile.name.clone());
            }
            if let Some(name) = library.get(key.as_str())
                && !aliases.iter().any(|a| a == name)
            {
                aliases.push(name.to_string());
            }
            let label = names.display(key);
            let owned = names.is_library_agent(key)
                || profile.is_some_and(|p| p.attested_owner.as_deref() == Some(inputs.viewer));
            Choice {
                key: key.clone(),
                name: aliases.first().cloned().unwrap_or_else(|| label.clone()),
                label,
                aliases,
                qualifier: names.qualifier(key).map(str::to_string),
                member: member_set.contains(key.as_str()),
                agent: names.is_agent(key),
                owned,
                managed: names.is_library_agent(key),
            }
        })
        .collect();
    let all_names: Vec<&str> = choices
        .iter()
        .flat_map(|c| {
            c.aliases
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(c.label.as_str()))
        })
        .collect();
    let teams: Vec<&str> = inputs.teams.iter().map(String::as_str).collect();
    let admitted = mention_rules::admitted(query, &all_names)
        || (!teams.is_empty() && mention_rules::admitted(query, &teams));
    if !admitted && !searching && !failed && inputs.roster_ready {
        return Completion {
            people: Vec::new(),
            status: None,
            retry: false,
            space: None,
            closed: true,
        };
    }
    let ranked = if admitted {
        mention_rules::rank(&choices, query, inputs.history, |key| {
            match inputs.presence.get(key) {
                Some(PresenceState::Online) => Presence::Online,
                Some(PresenceState::Away) => Presence::Away,
                _ => Presence::Unknown,
            }
        })
    } else {
        Vec::new()
    };
    let truncated = ranked.len() > LIMIT;
    let people: Vec<Person> = ranked
        .into_iter()
        .take(LIMIT)
        .map(|choice| Person {
            pubkey: choice.key.clone(),
            name: choice.name.clone(),
            label: choice.label.clone(),
            picture: inputs
                .profiles
                .get(choice.key.as_str())
                .and_then(|p| p.picture.clone()),
            agent: choice.agent,
            member: choice.member,
        })
        .collect();
    // Space (README section 5): once every member is named, an exact local
    // name (a member or an agent offered for mention) selects at once. Any
    // other exact name needs the full choice set: while the directory
    // search runs, failed, or has more pages, a namesake may be missing.
    let missing_names = members
        .iter()
        .any(|key| !inputs.profiles.contains_key(key.as_str()));
    let complete = !searching && !failed && !more;
    let space = (!missing_names)
        .then(|| mention_rules::exact_mention_among(&choices, query, |c| local.contains(&c.key)))
        .flatten()
        .or_else(|| {
            complete
                .then(|| mention_rules::exact_mention(&choices, query))
                .flatten()
        })
        .filter(|choice| people.iter().any(|p| p.pubkey == choice.key))
        .map(|choice| choice.key.clone());
    let missing_members = channel.is_some_and(|c| c.members.is_empty());
    let status = if !inputs.roster_ready {
        Some("Loading recipients…")
    } else if failed {
        Some("Could not search community people. Retry to refresh.")
    } else if !admitted {
        None
    } else if missing_members {
        Some("Channel membership unavailable.")
    } else if searching {
        Some("Searching community…")
    } else if more || truncated {
        Some("Narrow your search to see more members.")
    } else {
        None
    };
    Completion {
        people,
        status,
        retry: failed || (admitted && missing_members),
        space,
        closed: false,
    }
}

/// A saved team row (`useTeamMentions`): a shortcut to its agents, never
/// a new identity.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamRow {
    pub id: String,
    pub name: String,
    /// The agents a pick adds, in the team's order.
    pub recipients: Vec<String>,
    /// Why it cannot be picked, shown as its detail.
    pub disabled: Option<&'static str>,
    pub detail: String,
}

/// Saved teams matching `query`, by name then id, at most 20. `eligible`
/// says whether the draft may address a key (`mentionCandidates`), and
/// `member` whether it is in the conversation.
pub fn team_rows(
    teams: &[(String, String, Vec<String>)],
    query: &str,
    ready: bool,
    channel: Option<&ChannelSummary>,
    eligible: impl Fn(&str) -> bool,
    member: impl Fn(&str) -> bool,
) -> Vec<TeamRow> {
    let mut matching: Vec<&(String, String, Vec<String>)> = teams
        .iter()
        .filter(|(id, name, _)| {
            let choice = Choice {
                key: id.clone(),
                name: name.clone(),
                label: name.clone(),
                aliases: vec![name.clone()],
                agent: true,
                owned: true,
                ..Default::default()
            };
            mention_rules::mention_match(&choice, query) != u32::MAX
        })
        .collect();
    matching.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    matching.truncate(20);
    matching
        .into_iter()
        .map(|(id, name, agents)| {
            let recipients: Vec<String> =
                agents.iter().filter(|key| eligible(key)).cloned().collect();
            let disabled = if !ready {
                Some("Team changed or unavailable. Reopen to refresh.")
            } else if agents.is_empty() {
                Some("This team has no agents.")
            } else if agents.len() > 32 {
                Some("Teams can mention at most 32 agents.")
            } else if recipients.len() != agents.len() {
                Some("A team member is unavailable. Edit the team or refresh choices.")
            } else {
                None
            };
            let outside = recipients.iter().filter(|key| !member(key)).count();
            let consequence = match channel.and_then(|c| c.channel_type) {
                _ if outside == 0 => String::new(),
                Some(ChannelType::Dm) => format!("{outside} not in DM · Will not be notified"),
                Some(ChannelType::Session) => {
                    let parent = channel.is_some_and(|c| c.parent_channel_id.is_some());
                    format!(
                        "Adds {outside} to session{} when you send",
                        if parent { " and parent channel" } else { "" }
                    )
                }
                _ => format!("{outside} not in channel · Choose whether to add when you send"),
            };
            let detail = match disabled {
                Some(reason) => reason.to_string(),
                None => [
                    format!("Saved team · {} agents", recipients.len()),
                    consequence,
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            };
            TeamRow {
                id: id.clone(),
                name: name.clone(),
                recipients,
                disabled,
                detail,
            }
        })
        .collect()
}

/// A saved team with this name, or one that continues it, keeps Space
/// from picking a person (`blocksSpace`).
pub fn teams_block_space(names: &[String], query: &str) -> bool {
    let needle = mention_rules::normalize_name(query);
    names.iter().any(|name| {
        mention_rules::normalize_name(name) == needle || mention_rules::extends_name(name, &needle)
    })
}

/// Explicit picks per channel, newest highest, for 100 channels of 100
/// keys each (`mention-history.ts`).
#[derive(Default)]
pub struct History {
    channels: VecDeque<(String, HashMap<String, u64>)>,
    tick: u64,
}

impl History {
    pub fn get(&self, channel: &str) -> HashMap<String, u64> {
        self.channels
            .iter()
            .find(|(id, _)| id == channel)
            .map(|(_, keys)| keys.clone())
            .unwrap_or_default()
    }

    pub fn remember(&mut self, channel: &str, key: &str) {
        self.tick += 1;
        let index = self.channels.iter().position(|(id, _)| id == channel);
        let mut entry = match index {
            Some(index) => self.channels.remove(index).expect("index is in range"),
            None => (channel.to_string(), HashMap::new()),
        };
        entry.1.insert(key.to_string(), self.tick);
        if entry.1.len() > 100
            && let Some(oldest) = entry
                .1
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(k, _)| k.clone())
        {
            entry.1.remove(&oldest);
        }
        self.channels.push_back(entry);
        if self.channels.len() > 100 {
            self.channels.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::Profile;
    use core::prelude::v1::test;
    use std::sync::Arc;

    fn key(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn profiles(names: &[(String, &str)]) -> Profiles {
        Arc::new(
            names
                .iter()
                .map(|(k, n)| {
                    (
                        k.clone(),
                        Arc::new(Profile {
                            name: n.to_string(),
                            ..Default::default()
                        }),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn members_first_then_found_people_and_space_needs_a_complete_set() {
        let (alice, alicia, bob) = (key(1), key(2), key(3));
        let profiles = profiles(&[
            (alice.clone(), "Alice"),
            (alicia.clone(), "Alicia"),
            (bob.clone(), "Bob"),
        ]);
        let channel = ChannelSummary {
            id: "c".into(),
            channel_type: Some(ChannelType::Stream),
            members: vec![bob.clone(), alicia.clone()],
            ..Default::default()
        };
        let library = AgentLibrary::default();
        let presence = HashMap::new();
        let history = HashMap::new();
        let inputs = Inputs {
            channel: Some(&channel),
            roster_ready: true,
            profiles: &profiles,
            library: &library,
            viewer: &bob,
            origin: None,
            presence: &presence,
            history: &history,
            teams: &[],
            can_invite: true,
        };
        let found = vec![alice.clone()];
        let done = complete(
            &inputs,
            "ali",
            Directory::Done {
                people: &found,
                more: false,
            },
        );
        let order: Vec<&str> = done.people.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(order, ["Alicia", "Alice"]);
        assert!(done.people[0].member && !done.people[1].member);
        assert_eq!(done.status, None);
        // "alice" is exact and unique once the search is in.
        assert_eq!(
            complete(
                &inputs,
                "alice",
                Directory::Done {
                    people: &found,
                    more: false
                }
            )
            .space,
            Some(alice.clone())
        );
        let loading = complete(&inputs, "alice", Directory::Loading(&found));
        assert_eq!(
            (loading.space, loading.status),
            (None, Some("Searching community…"))
        );
        let failed = complete(&inputs, "al", Directory::Failed);
        assert!(failed.retry);
        assert_eq!(
            failed.status,
            Some("Could not search community people. Retry to refresh.")
        );
        // Prose after a complete name closes the popup.
        assert!(
            complete(
                &inputs,
                "Bob is ",
                Directory::Done {
                    people: &[],
                    more: false
                }
            )
            .closed
        );
    }

    #[test]
    fn space_selects_an_exact_member_without_waiting_for_the_directory() {
        let (viewer, jitter, other, outside, longer) = (key(1), key(2), key(3), key(4), key(5));
        let library = AgentLibrary::default();
        let (presence, history) = (HashMap::new(), HashMap::new());
        let space = |names: &[(String, &str)], members: Vec<String>, query, directory| {
            let profiles = profiles(names);
            let channel = ChannelSummary {
                id: "c".into(),
                channel_type: Some(ChannelType::Stream),
                members,
                ..Default::default()
            };
            let inputs = Inputs {
                channel: Some(&channel),
                roster_ready: true,
                profiles: &profiles,
                library: &library,
                viewer: &viewer,
                origin: None,
                presence: &presence,
                history: &history,
                teams: &[],
                can_invite: true,
            };
            complete(&inputs, query, directory).space
        };
        let named = [
            (viewer.clone(), "Me"),
            (jitter.clone(), "Jitter"),
            (other.clone(), "Other"),
            (outside.clone(), "Jitter"),
            (longer.clone(), "Jitter Bug"),
        ];
        let members = vec![viewer.clone(), jitter.clone(), other.clone()];
        let found = [outside.clone()];
        // Before the search answers, and whatever it finds after.
        for directory in [
            Directory::Loading(&[]),
            Directory::Done {
                people: &[],
                more: false,
            },
            Directory::Done {
                people: &found,
                more: true,
            },
            Directory::Failed,
        ] {
            assert_eq!(
                space(&named, members.clone(), "jitter", directory),
                Some(jitter.clone())
            );
        }
        // A member namesake keeps Space as prose.
        let twins = [
            (viewer.clone(), "Me"),
            (jitter.clone(), "Jitter"),
            (other.clone(), "Jitter"),
        ];
        assert_eq!(
            space(&twins, members.clone(), "jitter", Directory::Loading(&[])),
            None
        );
        // So does a member whose name is not known yet.
        assert_eq!(
            space(
                &named[..2],
                members.clone(),
                "jitter",
                Directory::Loading(&[])
            ),
            None
        );
        // A longer outside name blocks once the directory returns it.
        let bug = [longer.clone()];
        assert_eq!(
            space(
                &named,
                members.clone(),
                "jitter",
                Directory::Done {
                    people: &bug,
                    more: false
                }
            ),
            None
        );
        // An outside-only name still waits for the full directory.
        let only = [(viewer.clone(), "Me"), (outside.clone(), "Solo")];
        let solo = [outside.clone()];
        assert_eq!(
            space(
                &only,
                vec![viewer.clone()],
                "solo",
                Directory::Loading(&solo)
            ),
            None
        );
        assert_eq!(
            space(
                &only,
                vec![viewer.clone()],
                "solo",
                Directory::Done {
                    people: &solo,
                    more: false
                }
            ),
            Some(outside.clone())
        );
    }

    #[test]
    fn offers_the_viewers_own_agents_outside_the_channel_before_directory_people() {
        let (viewer, member, agent, aaron) = (key(1), key(2), key(3), key(4));
        let profiles = profiles(&[
            (viewer.clone(), "Me"),
            (member.clone(), "Member"),
            (aaron.clone(), "Aaron"),
        ]);
        let channel = ChannelSummary {
            id: "c".into(),
            channel_type: Some(ChannelType::Stream),
            members: vec![viewer.clone(), member.clone()],
            ..Default::default()
        };
        // An agent on a remote host, not in the channel yet.
        let library = AgentLibrary {
            agents: vec![(
                agent.clone(),
                "Outside agent".into(),
                "wss://relay.example".into(),
            )],
        };
        let (presence, history) = (HashMap::new(), HashMap::new());
        let inputs = Inputs {
            channel: Some(&channel),
            roster_ready: true,
            profiles: &profiles,
            library: &library,
            viewer: &viewer,
            origin: None,
            presence: &presence,
            history: &history,
            teams: &[],
            can_invite: true,
        };
        let found = [aaron.clone()];
        let done = complete(
            &inputs,
            "",
            Directory::Done {
                people: &found,
                more: false,
            },
        );
        let labels: Vec<&str> = done.people.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Me", "Member", "Outside agent", "Aaron"]);
    }

    /// `mentionCandidates`: outside agents only where a send can add them.
    #[test]
    fn offers_outside_agents_only_in_streams_and_forums_the_viewer_can_add_to() {
        let (viewer, member, agent) = (key(1), key(2), key(3));
        let profiles = profiles(&[(viewer.clone(), "Me"), (member.clone(), "Member")]);
        let library = AgentLibrary {
            agents: vec![(agent.clone(), "Scout".into(), "wss://relay.example".into())],
        };
        let (presence, history) = (HashMap::new(), HashMap::new());
        let labels = |channel_type, can_invite| {
            let channel = ChannelSummary {
                id: "c".into(),
                channel_type: Some(channel_type),
                members: vec![viewer.clone(), member.clone()],
                ..Default::default()
            };
            let inputs = Inputs {
                channel: Some(&channel),
                roster_ready: true,
                profiles: &profiles,
                library: &library,
                viewer: &viewer,
                origin: None,
                presence: &presence,
                history: &history,
                teams: &[],
                can_invite,
            };
            complete(&inputs, "", Directory::Idle)
                .people
                .into_iter()
                .map(|p| p.label)
                .collect::<Vec<_>>()
        };
        for channel_type in [ChannelType::Stream, ChannelType::Forum] {
            assert_eq!(labels(channel_type, true), ["Me", "Member", "Scout"]);
            assert_eq!(labels(channel_type, false), ["Me", "Member"]);
        }
        assert_eq!(labels(ChannelType::Dm, true), ["Me", "Member"]);
        // A session invites the viewer's agents.
        assert_eq!(
            labels(ChannelType::Session, false),
            ["Me", "Member", "Scout"]
        );
        assert!(!offers_agents(None, true));
    }

    #[test]
    fn history_keeps_recent_picks_per_channel() {
        let mut history = History::default();
        history.remember("c", "a");
        history.remember("c", "b");
        let picks = history.get("c");
        assert!(picks["b"] > picks["a"]);
        assert!(history.get("other").is_empty());
    }

    #[test]
    fn teams_match_by_name_and_explain_what_blocks_them() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let teams = vec![
            (
                "t2".to_string(),
                "Reviewers".to_string(),
                vec![a.clone(), b.clone()],
            ),
            (
                "t1".to_string(),
                "Release crew".to_string(),
                vec![a.clone()],
            ),
            ("t3".to_string(), "Empty".to_string(), Vec::new()),
        ];
        let channel = ChannelSummary {
            id: "c".into(),
            channel_type: Some(ChannelType::Stream),
            members: vec![a.clone()],
            ..Default::default()
        };
        let member = |key: &str| key == a;
        let rows = team_rows(&teams, "re", true, Some(&channel), |_| true, member);
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["Release crew", "Reviewers"]);
        assert_eq!(rows[0].detail, "Saved team · 1 agents");
        assert_eq!(
            rows[1].detail,
            "Saved team · 2 agents · 1 not in channel · Choose whether to add when you send"
        );
        let only_a = |key: &str| key == a;
        let rows = team_rows(&teams, "rev", true, Some(&channel), only_a, member);
        assert_eq!(
            rows[0].disabled,
            Some("A team member is unavailable. Edit the team or refresh choices.")
        );
        let rows = team_rows(&teams, "empty", true, Some(&channel), only_a, member);
        assert_eq!(rows[0].disabled, Some("This team has no agents."));
        let rows = team_rows(&teams, "rev", false, Some(&channel), only_a, member);
        assert_eq!(
            rows[0].disabled,
            Some("Team changed or unavailable. Reopen to refresh.")
        );
        let names: Vec<String> = teams.iter().map(|(_, name, _)| name.clone()).collect();
        assert!(teams_block_space(&names, "release"));
        assert!(!teams_block_space(&names, "rel"));
    }
}
