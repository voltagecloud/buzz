//! The owner's teams on this device (buzz-app's Agents teams): each a name,
//! instructions and agent members, given to every member's prompt with a
//! roster. Agents record nothing about their teams; deleting a team never
//! touches an agent, and a deleted agent drops out of every team.

pub use super::store::Team;
use crate::ui::keys::npub;

/// A team to save: `id` replaces that team, `None` makes a new one.
#[derive(Clone, Debug, Default)]
pub struct TeamChange {
    pub id: Option<String>,
    pub name: String,
    pub instructions: String,
    pub members: Vec<String>,
}

/// Validates `change` against the saved `teams` and the agents that exist
/// (`live`), and returns the team to save, with duplicate members removed.
pub(crate) fn validate(
    change: TeamChange,
    teams: &[Team],
    live: impl Fn(&str) -> bool,
) -> Result<Team, String> {
    let name = change.name.trim();
    if name.is_empty() {
        return Err("Name the team".into());
    }
    if change.members.iter().any(|pubkey| !live(pubkey)) {
        return Err("A member is not an agent on this device".into());
    }
    if let Some(id) = &change.id
        && !teams.iter().any(|team| &team.id == id)
    {
        return Err("No such team".into());
    }
    let mut members: Vec<String> = Vec::new();
    for pubkey in change.members {
        if !members.contains(&pubkey) {
            members.push(pubkey);
        }
    }
    Ok(Team {
        id: change
            .id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        name: name.to_owned(),
        instructions: change.instructions,
        members,
    })
}

/// The teams with `team` saved: in place of the one with its id, or last.
pub(crate) fn with(teams: &[Team], team: Team) -> Vec<Team> {
    let mut next = teams.to_vec();
    match next.iter_mut().find(|saved| saved.id == team.id) {
        Some(saved) => *saved = team,
        None => next.push(team),
    }
    next
}

/// The team sections for agent `me` (buzz-app's `teamPrompt`): each team
/// it is on, its members that still exist by name and npub, and the
/// team's instructions. `names` gives each existing agent's name.
pub fn team_prompt<'a>(
    teams: &[Team],
    names: impl Fn(&str) -> Option<&'a str>,
    me: &str,
) -> String {
    teams
        .iter()
        .filter(|team| team.members.iter().any(|pubkey| pubkey == me))
        .map(|team| {
            let mut lines = vec![
                "<team>".to_owned(),
                format!(
                    "You are on the team {}. Its members, you included:",
                    serde_json::Value::from(team.name.as_str())
                ),
            ];
            for pubkey in &team.members {
                let Some(name) = names(pubkey) else {
                    continue;
                };
                let you = if pubkey == me { ", you" } else { "" };
                lines.push(format!("- {name} ({}){you}", npub(pubkey)));
            }
            lines.push("Mention a teammate by name, with their npub, to reach them.".into());
            let instructions = team.instructions.trim();
            if !instructions.is_empty() {
                lines.push("<team-instructions>".into());
                lines.push(instructions.to_owned());
                lines.push("</team-instructions>".into());
            }
            lines.push("</team>".into());
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(id: &str, members: &[&str]) -> Team {
        Team {
            id: id.into(),
            name: format!("Team {id}"),
            instructions: String::new(),
            members: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    #[test]
    fn saving_validates_and_dedupes() {
        let a = "a".repeat(64);
        let live = |pubkey: &str| pubkey == "a".repeat(64);
        let change = |name: &str, members: Vec<String>| TeamChange {
            name: name.into(),
            members,
            ..Default::default()
        };
        assert_eq!(
            validate(change(" ", vec![]), &[], live).unwrap_err(),
            "Name the team"
        );
        assert_eq!(
            validate(change("X", vec!["b".repeat(64)]), &[], live).unwrap_err(),
            "A member is not an agent on this device"
        );
        let missing = TeamChange {
            id: Some("gone".into()),
            ..change("X", vec![])
        };
        assert_eq!(validate(missing, &[], live).unwrap_err(), "No such team");
        let saved = validate(change(" Crew ", vec![a.clone(), a.clone()]), &[], live).unwrap();
        assert_eq!(saved.name, "Crew");
        assert_eq!(saved.members, vec![a]);
        let renamed = Team {
            name: "Other".into(),
            ..saved.clone()
        };
        assert_eq!(with(&[saved], renamed.clone()), vec![renamed]);
    }

    #[test]
    fn the_prompt_lists_each_team_the_agent_is_on() {
        let (me, mate, gone) = ("1".repeat(64), "2".repeat(64), "3".repeat(64));
        let mut crew = team("c", &[&me, &mate, &gone]);
        crew.name = "Crew \"A\"".into();
        crew.instructions = "  Review each other.  ".into();
        let teams = [crew, team("x", &[&mate])];
        let names = |pubkey: &str| match pubkey {
            p if p == "1".repeat(64) => Some("Me"),
            p if p == "2".repeat(64) => Some("Mate"),
            _ => None,
        };
        assert_eq!(
            team_prompt(&teams, names, &me),
            format!(
                "<team>\nYou are on the team \"Crew \\\"A\\\"\". Its members, you included:\n- Me ({}), you\n- Mate ({})\nMention a teammate by name, with their npub, to reach them.\n<team-instructions>\nReview each other.\n</team-instructions>\n</team>",
                npub(&me),
                npub(&mate)
            )
        );
        assert_eq!(team_prompt(&teams, names, &"4".repeat(64)), "");
    }
}
