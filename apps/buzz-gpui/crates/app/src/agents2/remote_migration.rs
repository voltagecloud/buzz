//! Reviewed batch changes of existing identities to the Remote workspace type.
//! The journal precedes every host mutation; interrupted changes are reconciled
//! against their exact destination rather than blindly repeated.

use super::{Agent, CarryKey, RetypePreview, Status, store::Team};
use crate::agent_plugins::remote_workspace::{config::Config, config::MANAGER_PUBKEY};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::PathBuf;

/// The existing agent type; migration never creates another identity.
pub const TARGET: &str = "voltage.remote-workspace/sandbox";
/// A batch fits in one review and bounds host work and recovery state.
pub const LIMIT: usize = 32;
const JOURNAL_LIMIT: u64 = 4 * 1024 * 1024;

/// The identity and community that authorized the batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub owner: String,
    pub relay: String,
}

/// The authoritative inventory used for selection and before each change.
pub struct Snapshot {
    pub scope: Scope,
    pub ready: bool,
    pub agents: Vec<Agent>,
    pub teams: Vec<Team>,
}

/// Only the fields a provider change depends on; attention stays live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Before {
    pub pubkey: String,
    pub name: String,
    pub kind: String,
    pub config: serde_json::Value,
}

impl Before {
    fn of(agent: &Agent) -> Self {
        Self {
            pubkey: agent.pubkey.clone(),
            name: agent.name.clone(),
            kind: agent.agent_type.clone(),
            config: agent.config.clone(),
        }
    }
}

/// One reviewed change, including settings retained for recovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub before: Before,
    pub after: serde_json::Value,
    pub warnings: Vec<String>,
}

/// A saved team's roster is fenced, but its live instructions remain live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Roster {
    pub id: String,
    pub members: BTreeSet<String>,
}

/// Durable status; cancelling explicitly abandons the remaining changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Active,
    Complete,
    Cancelled,
}

/// A reviewable batch bound to its owner, sources, roster, and reference.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    version: u8,
    pub scope: Scope,
    pub roster: Option<Roster>,
    pub reference: Option<Before>,
    pub items: Vec<Item>,
    pub already_remote: Vec<String>,
    pub completed: BTreeSet<usize>,
    pub state: State,
}

/// The next action after reconciliation.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Complete,
    Reconcile(usize),
    Change(usize, Change),
}

/// A crash can leave the host's type changed before its local config saves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Retype,
    FinishConfig,
}

/// A persisted step can start a host request, reconcile one, or finish.
pub enum Step<T> {
    Complete,
    Reconciled,
    Started(usize, T),
}

/// What checked steps require from persistence, also used by failure tests.
pub trait Journal {
    fn save(&self, plan: &Plan) -> Result<(), String>;
}

/// A new configuration needs no already-remote reference agent.
pub fn defaults(scope: &Scope) -> Config {
    Config {
        provider_config: Config::relay(&scope.relay, MANAGER_PUBKEY),
        ..Config::default()
    }
}

impl Snapshot {
    fn agent(&self, pubkey: &str) -> Result<&Agent, String> {
        let agent = self
            .agents
            .iter()
            .find(|agent| agent.pubkey == pubkey)
            .ok_or("A selected agent is no longer available here.")?;
        if agent.owner != self.scope.owner || agent.relay != self.scope.relay {
            return Err("Select agents owned by this account in this community.".into());
        }
        Ok(agent)
    }

    /// Resolve an entire saved team by its exact member keys.
    pub fn roster(&self, id: &str) -> Result<Roster, String> {
        let team = self
            .teams
            .iter()
            .find(|team| team.id == id)
            .ok_or("The selected team no longer exists.")?;
        Ok(Roster {
            id: team.id.clone(),
            members: team.members.iter().cloned().collect(),
        })
    }
}

impl Plan {
    /// Build a batch from the same provider previews used by individual changes.
    /// Known harnesses keep their model options; other types explicitly review
    /// the selected destination runtime and any settings that stay behind.
    pub fn review(
        snapshot: &Snapshot,
        selected: &BTreeSet<String>,
        team: Option<&str>,
        reference: Option<&str>,
        destination: &Config,
        mut preview: impl FnMut(&str) -> Result<RetypePreview, String>,
    ) -> Result<Self, String> {
        if !snapshot.ready {
            return Err("Wait for the agent inventory to finish loading.".into());
        }
        if selected.is_empty() || selected.len() > LIMIT {
            return Err(format!("Select between 1 and {LIMIT} agents."));
        }
        destination.validate()?;
        let reference = reference
            .map(|key| -> Result<Before, String> {
                let agent = snapshot.agent(key)?;
                if agent.agent_type != TARGET {
                    return Err("The reference must be a Remote workspace agent.".into());
                }
                Ok(Before::of(agent))
            })
            .transpose()?;
        if let Some(reference) = &reference {
            let mut expected = Config::read(&reference.config);
            expected.env.clear();
            expected.instructions.clear();
            let mut supplied = destination.clone();
            supplied.env.clear();
            supplied.instructions.clear();
            if expected != supplied {
                return Err(
                    "The reference configuration changed. Select it again before reviewing.".into(),
                );
            }
        }
        let roster = team.map(|id| snapshot.roster(id)).transpose()?;
        if roster
            .as_ref()
            .is_some_and(|team| !selected.is_subset(&team.members))
        {
            return Err("The selected agents are no longer members of that team.".into());
        }
        let mut items = Vec::new();
        let mut already_remote = Vec::new();
        for pubkey in selected {
            let agent = snapshot.agent(pubkey)?;
            if agent.agent_type == TARGET {
                already_remote.push(agent.name.clone());
                continue;
            }
            let migration = preview(pubkey)?;
            let mut config = destination.clone();
            // Credentials belong in the enrolled owner profile. Neither local
            // harness logins nor a reference agent's private environment moves.
            config.env.clear();
            config.instructions = Config::read(&migration.config).instructions;
            if let Some(instructions) = agent.config["instructions"].as_str()
                && instructions != config.instructions
            {
                return Err(format!(
                    "{}'s instructions cannot be preserved. Restore its agent type before moving it.",
                    agent.name
                ));
            }
            let mut warnings = Vec::new();
            let runtime = match agent.agent_type.as_str() {
                "buzz.codex/codex" => Some("codex"),
                "buzz.claude-code/claude-code" => Some("claude"),
                _ => None,
            };
            if let Some(runtime) = runtime {
                config.runtime = runtime.into();
                config.model = agent.config["model"].as_str().unwrap_or_default().into();
                config.effort = agent.config["effort"].as_str().unwrap_or_default().into();
                config.fast = agent.config["fast"].as_bool().unwrap_or(false);
            } else {
                warnings.push(format!("Runtime changes to {}.", config.runtime));
            }
            if migration.dropped.contains(&CarryKey::Instructions) {
                return Err(format!(
                    "{}'s instructions cannot be preserved.",
                    agent.name
                ));
            }
            warnings.extend(
                migration
                    .dropped
                    .iter()
                    .map(|key| format!("{} stays local.", key.label())),
            );
            warnings.extend(
                migration
                    .settings
                    .iter()
                    .filter(|setting| {
                        runtime.is_none() || !matches!(setting.id, "model" | "thinking" | "fast")
                    })
                    .map(|setting| {
                        if setting.id == "sessions" {
                            "Sessions: each thread starts a separate remote sandbox.".into()
                        } else {
                            format!("{} is not copied.", setting.label)
                        }
                    }),
            );
            config.validate()?;
            items.push(Item {
                before: Before::of(agent),
                after: config.value(),
                warnings,
            });
        }
        Ok(Self {
            version: 1,
            scope: snapshot.scope.clone(),
            roster,
            reference,
            state: if items.is_empty() {
                State::Complete
            } else {
                State::Active
            },
            items,
            already_remote,
            completed: BTreeSet::new(),
        })
    }

    /// Reject stale sources before selecting another mutation. An exact
    /// destination recognizes a host write whose response was lost.
    pub fn next(&self, snapshot: &Snapshot) -> Result<Next, String> {
        if snapshot.scope != self.scope || !snapshot.ready {
            return Err("Return to the original account and community, then resume.".into());
        }
        if let Some(roster) = &self.roster
            && snapshot.roster(&roster.id)? != *roster
        {
            return Err("The team's roster changed. Review the remaining selection again.".into());
        }
        if let Some(reference) = &self.reference
            && Before::of(snapshot.agent(&reference.pubkey)?) != *reference
        {
            return Err(
                "The reference configuration changed. Review the remaining selection again.".into(),
            );
        }
        let mut next = None;
        for (index, item) in self.items.iter().enumerate() {
            let agent = snapshot.agent(&item.before.pubkey)?;
            let current = Before::of(agent);
            let after = Before {
                kind: TARGET.into(),
                config: item.after.clone(),
                ..item.before.clone()
            };
            let applied = current == after;
            let interrupted = current.kind == TARGET
                && current.config == item.before.config
                && current.name == item.before.name;
            if (!applied && !interrupted && current != item.before)
                || (self.completed.contains(&index) && !applied)
            {
                return Err(format!(
                    "{} changed after review. Review the remaining agents again.",
                    item.before.name
                ));
            }
            if next.is_none() && !self.completed.contains(&index) {
                next = Some(if applied {
                    Next::Reconcile(index)
                } else {
                    Next::Change(
                        index,
                        if interrupted {
                            Change::FinishConfig
                        } else {
                            Change::Retype
                        },
                    )
                });
            }
        }
        Ok(next.unwrap_or(Next::Complete))
    }
}

/// This is the production admission seam: persistence and stale-state checks
/// complete before the closure can start the existing single-agent operation.
pub fn checked_step<T>(
    plan: &mut Plan,
    snapshot: &Snapshot,
    journal: &impl Journal,
    change: impl FnOnce(&Item, Change) -> T,
) -> Result<Step<T>, String> {
    if plan.state != State::Active {
        return Ok(Step::Complete);
    }
    match plan.next(snapshot)? {
        Next::Complete => {
            plan.state = State::Complete;
            if let Err(error) = journal.save(plan) {
                plan.state = State::Active;
                return Err(error);
            }
            Ok(Step::Complete)
        }
        Next::Reconcile(index) => {
            plan.completed.insert(index);
            journal.save(plan)?;
            Ok(Step::Reconciled)
        }
        Next::Change(index, operation) => {
            journal.save(plan)?;
            Ok(Step::Started(index, change(&plan.items[index], operation)))
        }
    }
}

/// One bounded private journal per account/community, replaced atomically.
#[derive(Clone)]
pub struct FileJournal(PathBuf);

impl FileJournal {
    /// Select the journal without placing user-controlled strings in its path.
    pub fn new(directory: PathBuf, scope: &Scope) -> Self {
        let hash = Sha256::digest(format!("{}#{}", scope.owner, scope.relay).as_bytes());
        Self(directory.join(format!("remote-agent-migration-{hash:x}.json")))
    }

    /// Restore only an unfinished, valid operation for this exact account.
    pub fn load(&self, scope: &Scope) -> Result<Option<Plan>, String> {
        let file = match std::fs::File::open(&self.0) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Could not read the migration journal: {error}")),
        };
        let mut bytes = Vec::new();
        file.take(JOURNAL_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Could not read the migration journal: {error}"))?;
        if bytes.len() as u64 > JOURNAL_LIMIT {
            return Err("The migration journal exceeds 4 MiB.".into());
        }
        let plan: Plan = serde_json::from_slice(&bytes).map_err(
            |_| "The migration journal is invalid. Preserve it before starting another migration.",
        )?;
        if plan.version != 1
            || &plan.scope != scope
            || plan.items.len() > LIMIT
            || plan.already_remote.len() + plan.items.len() > LIMIT
            || plan
                .items
                .iter()
                .map(|item| &item.before.pubkey)
                .collect::<BTreeSet<_>>()
                .len()
                != plan.items.len()
            || plan
                .completed
                .iter()
                .any(|index| *index >= plan.items.len())
        {
            return Err("The migration journal does not match this account or version.".into());
        }
        for item in &plan.items {
            Config::read(&item.after).validate()?;
        }
        Ok((plan.state == State::Active).then_some(plan))
    }

    /// Keep an unreadable record for diagnosis while allowing a fresh review
    /// of the live inventory. This never modifies an agent.
    pub fn archive(&self) -> Result<(), String> {
        let directory = self
            .0
            .parent()
            .ok_or("The journal directory is unavailable.")?;
        let (_, backup) = tempfile::Builder::new()
            .prefix("remote-agent-migration-archived-")
            .suffix(".json")
            .tempfile_in(directory)
            .map_err(|error| format!("Could not create a migration archive: {error}"))?
            .keep()
            .map_err(|error| format!("Could not create a migration archive: {error}"))?;
        std::fs::rename(&self.0, backup)
            .map_err(|error| format!("Could not archive the migration record: {error}"))
    }
}

impl Journal for FileJournal {
    fn save(&self, plan: &Plan) -> Result<(), String> {
        let bytes =
            serde_json::to_vec(plan).map_err(|_| "Could not serialize the migration journal.")?;
        if bytes.len() as u64 > JOURNAL_LIMIT {
            return Err("The migration journal exceeds 4 MiB. Select fewer agents.".into());
        }
        let directory = self
            .0
            .parent()
            .ok_or("The journal directory is unavailable.")?;
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("Could not create the journal directory: {error}"))?;
        let mut file = tempfile::NamedTempFile::new_in(directory)
            .map_err(|error| format!("Could not create the private migration journal: {error}"))?;
        file.write_all(&bytes)
            .and_then(|()| file.as_file().sync_all())
            .map_err(|error| format!("Could not save the migration journal: {error}"))?;
        file.persist(&self.0)
            .map_err(|error| format!("Could not replace the migration journal: {}", error.error))?;
        #[cfg(unix)]
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("Could not sync the migration journal directory: {error}"))?;
        Ok(())
    }
}

/// Capture the same account/community boundary that the Agents page shows.
pub fn current_scope(cx: &gpui_kit::App) -> Result<Scope, String> {
    use crate::community::{AppState, IdentityState};
    let owner = match &cx.global::<AppState>().identity {
        IdentityState::Ready(identity) => identity.pubkey_hex(),
        _ => return Err("Sign in before moving agents.".into()),
    };
    let community = AppState::current(cx).ok_or("Select a community first.")?;
    let relay = community.read(cx).origin.websocket().to_owned();
    Ok(Scope { owner, relay })
}

/// Read the selected inventory with its authenticated community boundary.
pub fn snapshot(service: &super::Agents2, cx: &gpui_kit::App) -> Result<Snapshot, String> {
    Ok(Snapshot {
        scope: current_scope(cx)?,
        ready: *service.status() == Status::Ready,
        agents: service.agents().to_vec(),
        teams: service.teams().to_vec(),
    })
}

#[cfg(test)]
mod tests;
