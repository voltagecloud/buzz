//! A session's workspace (`sessions/workspace.ts`): project folders and
//! an optional new worktree, kept as ordinary Canvas instructions behind
//! a metadata line so both apps (and agents) read the same document.
//!
//! Also the template checks `channel-templates/model.ts` runs before a
//! group's defaults are published (`parseLineup`, `parseKitRecord`), so a
//! save fails with the same message as in buzz-app.

use buzz_relay_client::kit::me::{CANVAS_BYTES, RECORD_BYTES, RawKitEntry as KitEntry};
use buzz_relay_client::members::is_key;
use serde::Serialize;
use serde_json::{Value, json};

const PREFIX: &str = "<!-- buzz-session-workspace:v1 ";
const END: &str = "<!-- /buzz-session-workspace -->";

/// Where new work goes when it gets its own worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    pub location: String,
    pub base_branch: String,
}

impl Default for Worktree {
    /// What "Work in a new worktree" starts with.
    fn default() -> Self {
        Self {
            location: "~/.buzz/worktrees".into(),
            base_branch: "main".into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    pub folders: Vec<String>,
    pub worktree: Option<Worktree>,
    pub canvas: String,
}

/// The metadata line, in `JSON.stringify`'s field order.
#[derive(Serialize)]
struct Metadata<'a> {
    folders: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<&'a Worktree>,
}

/// UTF-16 length, as JavaScript counts `length`.
fn units(value: &str) -> usize {
    value.encode_utf16().count()
}

fn path(value: &str) -> bool {
    units(value) <= 2048 && value.chars().all(|c| c as u32 >= 32 && c != '`')
}

/// `validateWorkspace`: trimmed, distinct folders, a complete worktree.
pub fn validate_workspace(value: &Workspace) -> Result<Workspace, String> {
    let mut folders: Vec<String> = Vec::new();
    for folder in value.folders.iter().map(|f| f.trim()) {
        if !folder.is_empty() && !folders.iter().any(|known| known == folder) {
            folders.push(folder.to_owned());
        }
    }
    if folders.len() > 20 || folders.iter().any(|folder| !path(folder)) {
        return Err(
            "Use up to 20 project folders, without control characters or backticks.".into(),
        );
    }
    let worktree = value.worktree.as_ref().map(|worktree| Worktree {
        location: worktree.location.trim().to_owned(),
        base_branch: worktree.base_branch.trim().to_owned(),
    });
    if let Some(worktree) = &worktree
        && (folders.is_empty()
            || worktree.location.is_empty()
            || !path(&worktree.location)
            || worktree.base_branch.is_empty()
            || !path(&worktree.base_branch))
    {
        return Err("Choose a project folder, worktree location, and base branch.".into());
    }
    Ok(Workspace {
        folders,
        worktree,
        canvas: value.canvas.clone(),
    })
}

/// `workspaceCanvas`: the Canvas a workspace is saved as.
pub fn workspace_canvas(input: &Workspace) -> Result<String, String> {
    let value = validate_workspace(input)?;
    if value.canvas.len() > CANVAS_BYTES {
        return Err("Keep Canvas below 24 KiB.".into());
    }
    if value.folders.is_empty() {
        return Ok(value.canvas);
    }
    let metadata = serde_json::to_string(&Metadata {
        folders: &value.folders,
        worktree: value.worktree.as_ref(),
    })
    .expect("metadata serializes")
    .replace('<', "\\u003c")
    .replace('>', "\\u003e");
    let mut lines = vec![format!("{PREFIX}{metadata} -->")];
    match &value.worktree {
        Some(worktree) => {
            lines.push("## Workspace instructions".into());
            lines.push(String::new());
            for folder in &value.folders {
                lines.push(format!(
                    "- Use `{folder}` for review only. Do not make changes directly in it."
                ));
            }
            lines.push(format!(
                "- Before starting new work, fetch the latest `{}` branch from origin.",
                worktree.base_branch
            ));
            lines.push(format!(
                "- For each repository that needs changes, create a new worktree under `{}` from `origin/{}`.",
                worktree.location, worktree.base_branch
            ));
            lines.push("- Make all changes in that new worktree.".into());
        }
        None => {
            lines.push("## Project folders".into());
            lines.push(String::new());
            for folder in &value.folders {
                lines.push(format!("- `{folder}`"));
            }
        }
    }
    lines.push(END.into());
    lines.push(String::new());
    lines.push(value.canvas);
    let canvas = lines.join("\n");
    if canvas.len() > CANVAS_BYTES {
        return Err("Keep Canvas and workspace instructions below 24 KiB.".into());
    }
    Ok(canvas)
}

/// `readWorkspace`: the workspace a Canvas holds. Fails when the managed
/// block is damaged or was edited by hand, so the caller can offer the
/// raw Canvas instead of silently dropping those edits.
pub fn read_workspace(content: &str) -> Result<Workspace, String> {
    if !content.starts_with(PREFIX) {
        return Ok(Workspace {
            canvas: content.to_owned(),
            ..Workspace::default()
        });
    }
    let block = format!("\n{END}\n\n");
    let line_end = content.find(" -->\n");
    let block_end = line_end.and_then(|at| content[at..].find(&block).map(|offset| at + offset));
    let (Some(line_end), Some(block_end)) = (line_end, block_end) else {
        return Err("Workspace metadata is incomplete. Edit the raw Canvas before changing workspace settings.".into());
    };
    let raw: Value = content
        .get(PREFIX.len()..line_end)
        .and_then(|json| serde_json::from_str(json).ok())
        .ok_or_else(|| {
            "Workspace metadata is invalid. Edit the raw Canvas before changing workspace settings."
                .to_owned()
        })?;
    let folders = raw
        .as_object()
        .and_then(|raw| raw.get("folders"))
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or("Invalid workspace folders.")?;
    let worktree = match raw.get("worktree") {
        None => None,
        Some(worktree) => {
            let field = |name| worktree.as_object()?.get(name)?.as_str().map(str::to_owned);
            match (field("location"), field("baseBranch")) {
                (Some(location), Some(base_branch)) => Some(Worktree {
                    location,
                    base_branch,
                }),
                _ => return Err("Invalid worktree settings.".into()),
            }
        }
    };
    let value = validate_workspace(&Workspace {
        folders,
        worktree,
        canvas: content[block_end + block.len()..].to_owned(),
    })?;
    if workspace_canvas(&value)? != content {
        return Err("Workspace instructions were edited in Canvas. Use the raw Canvas editor to preserve those changes.".into());
    }
    Ok(value)
}

// ----- Templates (`channel-templates/model.ts`) -----

/// A template's agents, teams and Canvas (`Lineup`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lineup {
    pub team_ids: Vec<String>,
    pub agents: Vec<String>,
    pub canvas: String,
}

impl Lineup {
    /// The lineup of a record's value or a saved draft (`parseLineup`).
    pub fn parse(raw: &Value) -> Result<Self, String> {
        let raw = raw.as_object().ok_or("Invalid channel recipe")?;
        let canvas = raw
            .get("canvas")
            .and_then(Value::as_str)
            .filter(|canvas| units(canvas) <= CANVAS_BYTES)
            .ok_or("Invalid channel recipe text")?;
        if canvas.len() > CANVAS_BYTES {
            return Err("Canvas is too large (24 KiB maximum)".into());
        }
        Ok(Self {
            team_ids: keys(raw.get("teamIds"), 100, identifier)?,
            agents: keys(raw.get("agents"), 200, is_key)?,
            canvas: canvas.to_owned(),
        })
    }

    pub fn to_value(&self) -> Value {
        json!({ "teamIds": self.team_ids, "agents": self.agents, "canvas": self.canvas })
    }
}

fn identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn keys(value: Option<&Value>, max: usize, valid: fn(&str) -> bool) -> Result<Vec<String>, String> {
    let invalid = || "Invalid channel recipe selection".to_owned();
    let list = value.and_then(Value::as_array).ok_or_else(invalid)?;
    if list.len() > max {
        return Err(invalid());
    }
    let mut keys: Vec<String> = Vec::with_capacity(list.len());
    for key in list {
        match key.as_str() {
            Some(key) if valid(key) && !keys.iter().any(|k| k == key) => keys.push(key.into()),
            _ => return Err(invalid()),
        }
    }
    Ok(keys)
}

/// The template record holding a group's defaults, as the dialog saves
/// it: the lineup's agents and teams with the new Canvas.
pub fn section_template(id: &str, section_name: &str, lineup: &Lineup, canvas: &str) -> Value {
    // `name.slice(0, 100)`: at most 100 UTF-16 units.
    let mut name = String::new();
    for c in section_name.chars() {
        if units(&name) + c.len_utf16() > 100 {
            break;
        }
        name.push(c);
    }
    json!({
        "teamIds": lineup.team_ids,
        "agents": lineup.agents,
        "canvas": canvas,
        "type": "template",
        "id": id,
        "name": format!("{name} chats"),
        "description": "Starting workspace and Canvas for new chats in this project.",
    })
}

/// `parseKitRecord` for a template value: the checks and messages
/// buzz-app runs before publishing a group's defaults, including the
/// 16 KiB record budget.
pub fn check_template(value: &Value) -> Result<(), String> {
    let text = |name: &str, max: usize, nonempty: bool| {
        value[name]
            .as_str()
            .filter(|s| units(s) <= max && (!nonempty || !s.trim().is_empty()))
            .map(|_| ())
            .ok_or_else(|| "Invalid channel recipe text".to_owned())
    };
    if !value["id"].as_str().is_some_and(identifier) {
        return Err("Invalid channel recipe ID".into());
    }
    text("name", 120, true)?;
    text("description", 1000, false)?;
    Lineup::parse(value)?;
    let record = json!({ "version": 1, "community": "", "deleted": false, "value": value });
    if serde_json::to_vec(&record).map_or(true, |bytes| bytes.len() > RECORD_BYTES) {
        return Err("This saved recipe exceeds 16 KiB; shorten its Canvas or selections".into());
    }
    Ok(())
}

/// The catalog's templates a group can start from: live, and not
/// another group's own defaults.
pub fn starting_templates(entries: &[KitEntry]) -> Vec<&KitEntry> {
    entries
        .iter()
        .filter(|entry| {
            !entry.deleted()
                && entry.kind() == "template"
                && !entry.id().starts_with("session-section-")
                && !entry.id().starts_with("me-section-")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(folders: &[&str], worktree: Option<(&str, &str)>, canvas: &str) -> Workspace {
        Workspace {
            folders: folders.iter().map(|f| f.to_string()).collect(),
            worktree: worktree.map(|(location, base_branch)| Worktree {
                location: location.into(),
                base_branch: base_branch.into(),
            }),
            canvas: canvas.into(),
        }
    }

    #[test]
    fn plain_canvas_needs_no_metadata() {
        let value = workspace(&[], None, "Just context");
        assert_eq!(workspace_canvas(&value).unwrap(), "Just context");
        assert_eq!(read_workspace("Just context").unwrap(), value);
        // Blank folders count as none.
        assert_eq!(
            workspace_canvas(&workspace(&["", "  "], None, "x")).unwrap(),
            "x"
        );
    }

    #[test]
    fn folders_encode_exactly_as_buzz_app() {
        let canvas = workspace_canvas(&workspace(
            &[" /repo ", "/other", "/repo", ""],
            None,
            "Keep this context",
        ))
        .unwrap();
        assert_eq!(
            canvas,
            "<!-- buzz-session-workspace:v1 {\"folders\":[\"/repo\",\"/other\"]} -->\n\
             ## Project folders\n\n- `/repo`\n- `/other`\n\
             <!-- /buzz-session-workspace -->\n\nKeep this context"
        );
        assert_eq!(
            read_workspace(&canvas).unwrap(),
            workspace(&["/repo", "/other"], None, "Keep this context")
        );
    }

    #[test]
    fn worktrees_encode_exactly_as_buzz_app() {
        let value = workspace(&["~/a<b>"], Some((" ~/.buzz/worktrees ", "develop")), "");
        let canvas = workspace_canvas(&value).unwrap();
        assert_eq!(
            canvas,
            "<!-- buzz-session-workspace:v1 {\"folders\":[\"~/a\\u003cb\\u003e\"],\"worktree\":{\"location\":\"~/.buzz/worktrees\",\"baseBranch\":\"develop\"}} -->\n\
             ## Workspace instructions\n\n\
             - Use `~/a<b>` for review only. Do not make changes directly in it.\n\
             - Before starting new work, fetch the latest `develop` branch from origin.\n\
             - For each repository that needs changes, create a new worktree under `~/.buzz/worktrees` from `origin/develop`.\n\
             - Make all changes in that new worktree.\n\
             <!-- /buzz-session-workspace -->\n\n"
        );
        let read = read_workspace(&canvas).unwrap();
        assert_eq!(read.folders, ["~/a<b>"]);
        assert_eq!(read.worktree.unwrap().location, "~/.buzz/worktrees");
        // A Canvas that itself mentions the end marker still round-trips.
        let tricky = workspace(&["/r"], None, "<!-- /buzz-session-workspace -->\n\nmore");
        let canvas = workspace_canvas(&tricky).unwrap();
        assert_eq!(read_workspace(&canvas).unwrap(), tricky);
    }

    #[test]
    fn validation_errors_match_buzz_app() {
        let folders = "Use up to 20 project folders, without control characters or backticks.";
        let many: Vec<String> = (0..21).map(|i| format!("/r{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            validate_workspace(&workspace(&many, None, "")).unwrap_err(),
            folders
        );
        assert_eq!(
            validate_workspace(&workspace(&many[..20], None, ""))
                .unwrap()
                .folders
                .len(),
            20
        );
        assert_eq!(
            validate_workspace(&workspace(&["/a`b"], None, "")).unwrap_err(),
            folders
        );
        assert_eq!(
            validate_workspace(&workspace(&["/a\tb"], None, "")).unwrap_err(),
            folders
        );
        let long = "a".repeat(2049);
        assert_eq!(
            validate_workspace(&workspace(&[&long], None, "")).unwrap_err(),
            folders
        );
        assert!(validate_workspace(&workspace(&[&long[1..]], None, "")).is_ok());
        let worktree = "Choose a project folder, worktree location, and base branch.";
        for value in [
            workspace(&[], Some(("~/w", "main")), ""),
            workspace(&["/r"], Some((" ", "main")), ""),
            workspace(&["/r"], Some(("~/w", "")), ""),
            workspace(&["/r"], Some(("~/w`", "main")), ""),
            workspace(&["/r"], Some(("~/w", "ma\u{1}in")), ""),
        ] {
            assert_eq!(validate_workspace(&value).unwrap_err(), worktree);
        }
    }

    #[test]
    fn canvas_budget_is_24_kib() {
        let full = "é".repeat(CANVAS_BYTES / 2);
        assert_eq!(
            workspace_canvas(&workspace(&[], None, &full)).unwrap(),
            full
        );
        let over = format!("{full}x");
        assert_eq!(
            workspace_canvas(&workspace(&[], None, &over)).unwrap_err(),
            "Keep Canvas below 24 KiB."
        );
        assert_eq!(
            workspace_canvas(&workspace(&["/repo"], None, &full)).unwrap_err(),
            "Keep Canvas and workspace instructions below 24 KiB."
        );
    }

    #[test]
    fn damaged_metadata_is_refused() {
        let canvas = workspace_canvas(&workspace(&["/repo"], None, "Body")).unwrap();
        let incomplete = "Workspace metadata is incomplete. Edit the raw Canvas before changing workspace settings.";
        assert_eq!(read_workspace(PREFIX).unwrap_err(), incomplete);
        assert_eq!(
            read_workspace(&canvas.replace(END, "<!-- end -->")).unwrap_err(),
            incomplete
        );
        let invalid = "Workspace metadata is invalid. Edit the raw Canvas before changing workspace settings.";
        let tail = format!("\n## Project folders\n{END}\n\n");
        assert_eq!(
            read_workspace(&format!("{PREFIX}{{nope -->{tail}")).unwrap_err(),
            invalid
        );
        // The line ends inside the prefix's own trailing space.
        assert_eq!(
            read_workspace(&format!("{PREFIX}-->{tail}")).unwrap_err(),
            invalid
        );
        for json in [
            "[]",
            "null",
            "{}",
            "{\"folders\":[1]}",
            "{\"folders\":\"/a\"}",
        ] {
            assert_eq!(
                read_workspace(&format!("{PREFIX}{json} -->{tail}")).unwrap_err(),
                "Invalid workspace folders."
            );
        }
        for worktree in [
            "null",
            "[]",
            "{\"location\":\"~/w\"}",
            "{\"location\":1,\"baseBranch\":\"main\"}",
        ] {
            let json = format!("{{\"folders\":[\"/a\"],\"worktree\":{worktree}}}");
            assert_eq!(
                read_workspace(&format!("{PREFIX}{json} -->{tail}")).unwrap_err(),
                "Invalid worktree settings."
            );
        }
        assert_eq!(
            read_workspace(&format!("{PREFIX}{{\"folders\":[\"/a`\"]}} -->{tail}")).unwrap_err(),
            "Use up to 20 project folders, without control characters or backticks."
        );
        assert_eq!(
            read_workspace(&canvas.replace("- `/repo`", "- `/repo` (main)")).unwrap_err(),
            "Workspace instructions were edited in Canvas. Use the raw Canvas editor to preserve those changes."
        );
    }

    #[test]
    fn lineups_parse_like_buzz_app() {
        let agent = "a".repeat(64);
        let lineup =
            Lineup::parse(&json!({"teamIds": ["t-1"], "agents": [agent], "canvas": "c"})).unwrap();
        assert_eq!(lineup.team_ids, ["t-1"]);
        assert_eq!(Lineup::parse(&lineup.to_value()).unwrap(), lineup);
        assert_eq!(
            Lineup::parse(&json!({"teamIds": [], "agents": ["A"], "canvas": ""})).unwrap_err(),
            "Invalid channel recipe selection"
        );
        assert_eq!(
            Lineup::parse(&json!({"teamIds": ["x", "x"], "agents": [], "canvas": ""})).unwrap_err(),
            "Invalid channel recipe selection"
        );
        assert_eq!(
            Lineup::parse(&json!({"teamIds": [], "agents": [], "canvas": 1})).unwrap_err(),
            "Invalid channel recipe text"
        );
        let big = "é".repeat(CANVAS_BYTES / 2 + 1);
        assert_eq!(
            Lineup::parse(&json!({"teamIds": [], "agents": [], "canvas": big})).unwrap_err(),
            "Canvas is too large (24 KiB maximum)"
        );
        assert_eq!(
            Lineup::parse(&json!([])).unwrap_err(),
            "Invalid channel recipe"
        );
    }

    #[test]
    fn section_templates_fit_the_record_budget() {
        let id = format!("me-section-{}", "0".repeat(64));
        let value = section_template(&id, "Launch", &Lineup::default(), "Work carefully");
        assert_eq!(value["name"], "Launch chats");
        assert_eq!(
            value["description"],
            "Starting workspace and Canvas for new chats in this project."
        );
        check_template(&value).unwrap();
        // The name keeps at most 100 UTF-16 units of the group's name.
        let long = section_template(&id, &"😀".repeat(60), &Lineup::default(), "");
        assert_eq!(long["name"], format!("{} chats", "😀".repeat(50)));
        let full = section_template(&id, "Launch", &Lineup::default(), &"x".repeat(16 * 1024));
        assert_eq!(
            check_template(&full).unwrap_err(),
            "This saved recipe exceeds 16 KiB; shorten its Canvas or selections"
        );
    }
}
