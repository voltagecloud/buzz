//! The owner's channel templates on this device (buzz-app's
//! `features/channel-templates/local.ts`): a name, the agents and Agents
//! teams a new channel starts with, and its starting Canvas. Creating a
//! channel from one sends ordinary channel setup, so the session owns
//! applying it.
//!
//! buzz-app keeps them in `localStorage["buzz.channel-templates.v1"]`;
//! here they are a JSON file of the same shape in the data directory.

use crate::agents2::store::Storage;
use crate::agents2::teams::Team;
use buzz_relay_client::kit::CANVAS_BYTES;
use gpui_kit::{App, AppContext, Context, Entity, Global};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

pub const TEMPLATES_KEY: &str = "buzz.channel-templates.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChannelTemplate {
    pub id: String,
    pub name: String,
    /// Agents team ids. A team deleted since is skipped.
    pub teams: Vec<String>,
    /// Agent pubkeys, added alongside the teams' members.
    pub agents: Vec<String>,
    pub canvas: String,
}

/// A template to save: `id` replaces that template, `None` makes a new
/// one.
#[derive(Clone, Debug, Default)]
pub struct TemplateChange {
    pub id: Option<String>,
    pub name: String,
    pub teams: Vec<String>,
    pub agents: Vec<String>,
    pub canvas: String,
}

/// Reads the stored text, skipping anything that is not a template.
fn parse(raw: Option<&str>) -> Vec<ChannelTemplate> {
    let Some(stored) = raw.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
        return Vec::new();
    };
    if stored.get("version") != Some(&Value::from(1)) {
        return Vec::new();
    }
    let Some(templates) = stored.get("templates").and_then(Value::as_array) else {
        return Vec::new();
    };
    let strings = |items: &Vec<Value>| -> Vec<String> {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    templates
        .iter()
        .filter_map(|item| {
            let text = |key: &str| item.get(key)?.as_str().map(str::to_owned);
            Some(ChannelTemplate {
                id: text("id")?,
                name: text("name")?,
                canvas: text("canvas")?,
                teams: strings(item.get("teams")?.as_array()?),
                // Templates saved before agents could be chosen have none.
                agents: item
                    .get("agents")
                    .and_then(Value::as_array)
                    .map(strings)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Each item once, in first-seen order.
fn unique(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// Validates `change` against the saved templates and returns them with
/// it saved: in place of the one with its id, or last.
fn saved_with(
    change: TemplateChange,
    saved: &[ChannelTemplate],
) -> Result<(ChannelTemplate, Vec<ChannelTemplate>), String> {
    let name = change.name.trim();
    if name.is_empty() {
        return Err("Name the template".into());
    }
    if change.canvas.len() > CANVAS_BYTES {
        return Err("Canvas is too large (24 KiB maximum)".into());
    }
    if let Some(id) = &change.id
        && !saved.iter().any(|item| &item.id == id)
    {
        return Err("No such template".into());
    }
    let template = ChannelTemplate {
        id: change
            .id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        name: name.to_owned(),
        teams: unique(change.teams),
        agents: unique(change.agents),
        canvas: if change.canvas.trim().is_empty() {
            String::new()
        } else {
            change.canvas
        },
    };
    let mut next = saved.to_vec();
    match next.iter_mut().find(|item| item.id == template.id) {
        Some(item) => *item = template.clone(),
        None => next.push(template.clone()),
    }
    Ok((template, next))
}

/// The agents a template adds: its own agents and each member of its
/// teams, once, that is one of the owner's agents in this community
/// (`is_agent`).
pub fn template_agents(
    template: &ChannelTemplate,
    teams: &[Team],
    is_agent: impl Fn(&str) -> bool,
) -> Vec<String> {
    let members = template.teams.iter().flat_map(|id| {
        teams
            .iter()
            .find(|team| &team.id == id)
            .map(|team| team.members.clone())
            .unwrap_or_default()
    });
    unique(template.agents.iter().cloned().chain(members).collect())
        .into_iter()
        .filter(|pubkey| is_agent(pubkey))
        .collect()
}

/// A JSON file, replaced atomically.
struct FileStorage(PathBuf);

impl Storage for FileStorage {
    fn read(&self) -> Option<String> {
        std::fs::read_to_string(&self.0).ok()
    }

    fn write(&self, text: &str) -> Result<(), String> {
        crate::atomic_file::write(&self.0, text.as_bytes())
            .map_err(|_| "Channel templates could not be saved".to_string())
    }
}

/// The templates, read once: this process is the file's only writer.
/// Observers hear each change once storage has taken it.
pub struct ChannelTemplates {
    storage: Arc<dyn Storage>,
    templates: Vec<ChannelTemplate>,
}

struct ChannelTemplatesGlobal(Entity<ChannelTemplates>);

impl Global for ChannelTemplatesGlobal {}

pub fn init(cx: &mut App) {
    let path = crate::community::data_dir().join(format!("{TEMPLATES_KEY}.json"));
    let storage: Arc<dyn Storage> = Arc::new(FileStorage(path));
    let templates = cx.new(|_| ChannelTemplates::new(storage));
    cx.set_global(ChannelTemplatesGlobal(templates));
}

impl ChannelTemplates {
    fn new(storage: Arc<dyn Storage>) -> Self {
        let templates = parse(storage.read().as_deref());
        Self { storage, templates }
    }

    pub fn global(cx: &App) -> Option<Entity<ChannelTemplates>> {
        cx.try_global::<ChannelTemplatesGlobal>()
            .map(|global| global.0.clone())
    }

    /// The saved templates, in the order they were made.
    pub fn templates(&self) -> &[ChannelTemplate] {
        &self.templates
    }

    /// Creates a template, or replaces the one with `change.id`.
    pub fn save(
        &mut self,
        change: TemplateChange,
        cx: &mut Context<Self>,
    ) -> Result<ChannelTemplate, String> {
        let template = self.apply_save(change)?;
        cx.notify();
        Ok(template)
    }

    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        self.apply_remove(id)?;
        cx.notify();
        Ok(())
    }

    fn apply_save(&mut self, change: TemplateChange) -> Result<ChannelTemplate, String> {
        let (template, next) = saved_with(change, &self.templates)?;
        self.write(next)?;
        Ok(template)
    }

    fn apply_remove(&mut self, id: &str) -> Result<(), String> {
        let next = self
            .templates
            .iter()
            .filter(|item| item.id != id)
            .cloned()
            .collect();
        self.write(next)
    }

    /// Saves the templates; memory changes only once storage has taken
    /// them.
    fn write(&mut self, templates: Vec<ChannelTemplate>) -> Result<(), String> {
        let text = serde_json::json!({ "version": 1, "templates": templates }).to_string();
        self.storage.write(&text)?;
        self.templates = templates;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents2::store::MemoryStorage;
    use core::prelude::v1::test;
    use serde_json::json;

    fn change(name: &str) -> TemplateChange {
        TemplateChange {
            name: name.into(),
            ..Default::default()
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn saves_replaces_and_removes_templates() {
        let storage = Arc::new(MemoryStorage::default());
        let mut templates = ChannelTemplates::new(storage.clone());
        let made = templates
            .apply_save(TemplateChange {
                name: "  Launch ".into(),
                teams: strings(&["t1", "t1"]),
                agents: strings(&["a", "a"]),
                canvas: "# Goal".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(made.name, "Launch");
        assert_eq!(made.teams, ["t1"]);
        assert_eq!(made.agents, ["a"]);
        assert_eq!(templates.templates(), std::slice::from_ref(&made));
        // What was written reads back the same.
        assert_eq!(
            ChannelTemplates::new(storage.clone()).templates(),
            std::slice::from_ref(&made)
        );
        templates
            .apply_save(TemplateChange {
                id: Some(made.id.clone()),
                name: "Ship".into(),
                ..Default::default()
            })
            .unwrap();
        let names: Vec<&str> = templates
            .templates()
            .iter()
            .map(|item| item.name.as_str())
            .collect();
        assert_eq!(names, ["Ship"]);
        templates.apply_remove(&made.id).unwrap();
        assert!(templates.templates().is_empty());
        assert!(ChannelTemplates::new(storage).templates().is_empty());
    }

    #[test]
    fn rejects_an_unnamed_template_an_unknown_id_and_an_oversized_canvas() {
        let mut templates = ChannelTemplates::new(Arc::new(MemoryStorage::default()));
        assert_eq!(
            templates.apply_save(change(" ")).unwrap_err(),
            "Name the template"
        );
        assert_eq!(
            templates
                .apply_save(TemplateChange {
                    id: Some("nope".into()),
                    ..change("A")
                })
                .unwrap_err(),
            "No such template"
        );
        assert_eq!(
            templates
                .apply_save(TemplateChange {
                    canvas: "x".repeat(24 * 1024 + 1),
                    ..change("A")
                })
                .unwrap_err(),
            "Canvas is too large (24 KiB maximum)"
        );
        assert!(templates.templates().is_empty());
        let blank = templates
            .apply_save(TemplateChange {
                canvas: " \n ".into(),
                ..change("Blank")
            })
            .unwrap();
        assert_eq!(blank.canvas, "");
    }

    #[test]
    fn skips_what_is_not_a_template_and_reads_older_ones_without_agents() {
        let text = json!({
            "version": 1,
            "templates": [
                {"id": "x", "name": "There", "teams": ["t", 3], "agents": ["a", 4], "canvas": ""},
                // Saved before templates had agents.
                {"id": "z", "name": "Older", "teams": ["t"], "canvas": ""},
                {"id": "y", "name": "Broken"},
            ],
        })
        .to_string();
        let template = |id: &str, name: &str, agents: &[&str]| ChannelTemplate {
            id: id.into(),
            name: name.into(),
            teams: strings(&["t"]),
            agents: strings(agents),
            canvas: String::new(),
        };
        assert_eq!(
            parse(Some(&text)),
            [template("x", "There", &["a"]), template("z", "Older", &[])]
        );
        assert!(parse(Some(r#"{"version":2,"templates":[]}"#)).is_empty());
        assert!(parse(Some("not json")).is_empty());
        assert!(parse(None).is_empty());
    }

    #[test]
    fn adds_its_agents_and_each_team_member_once_if_one_of_the_owners() {
        let team = |id: &str, members: &[&str]| Team {
            id: id.into(),
            name: id.into(),
            instructions: String::new(),
            members: strings(members),
        };
        let teams = [team("t1", &["a", "b"]), team("t2", &["b", "c"])];
        let template = ChannelTemplate {
            id: "x".into(),
            name: "X".into(),
            teams: strings(&["t2", "t1", "gone"]),
            agents: strings(&["elsewhere", "d", "a"]),
            canvas: String::new(),
        };
        let mine = ["a", "b", "d"];
        assert_eq!(
            template_agents(&template, &teams, |pubkey| mine.contains(&pubkey)),
            ["d", "a", "b"]
        );
    }
}
