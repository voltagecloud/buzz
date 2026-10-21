//! The type's config and Codex's model catalog (buzz-app's `config.ts`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where Codex works unless an agent names another directory.
pub const DEFAULT_WORKSPACE: &str = "~/.buzz";

/// The type's config, as buzz-app saves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// A Codex model; empty uses Codex's own default.
    pub model: String,
    /// A reasoning effort; empty uses the model's default.
    pub effort: String,
    pub instructions: String,
    /// An absolute directory, or one under the home directory written
    /// `~/…`.
    pub workspace: String,
    /// `thread` or `channel`: whether each thread is its own session, or
    /// the whole channel is one.
    pub scope: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            effort: String::new(),
            instructions: String::new(),
            workspace: DEFAULT_WORKSPACE.into(),
            scope: "thread".into(),
        }
    }
}

impl Config {
    /// The saved config, each field checked; a blank workspace is the
    /// default.
    pub fn read(value: &Value) -> Self {
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let workspace = field("workspace").trim().to_string();
        Self {
            model: field("model"),
            effort: field("effort"),
            instructions: field("instructions"),
            workspace: if workspace.is_empty() {
                DEFAULT_WORKSPACE.into()
            } else {
                workspace
            },
            scope: if field("scope") == "channel" {
                "channel".into()
            } else {
                "thread".into()
            },
        }
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    pub fn channel_scope(&self) -> bool {
        self.scope == "channel"
    }
}

/// One reasoning effort a model offers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Effort {
    pub reasoning_effort: String,
    pub description: String,
}

/// One model as `model/list` describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Model {
    pub model: String,
    pub display_name: String,
    pub description: String,
    pub is_default: bool,
    pub default_reasoning_effort: String,
    pub supported_reasoning_efforts: Vec<Effort>,
}

/// How the settings name a reasoning effort.
pub fn effort_name(effort: &str) -> String {
    match effort {
        "xhigh" => "Extra high".into(),
        "max" => "Maximum".into(),
        "ultra" => "Ultra".into(),
        _ => {
            let mut chars = effort.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

/// An absolute path on the host: `/…`, or a Windows drive.
pub fn absolute_workspace(path: &str) -> bool {
    let path = path.trim();
    let bytes = path.as_bytes();
    path.starts_with('/')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// A workspace Codex can start in: absolute, or under the home directory
/// written `~/…`, which the process host expands.
pub fn valid_workspace(path: &str) -> bool {
    let path = path.trim();
    path == "~" || path.starts_with("~/") || absolute_workspace(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_saved_config_over_the_defaults() {
        assert_eq!(Config::read(&json!({})), Config::default());
        let saved = Config::read(&json!({
            "model": "gpt", "effort": "high", "instructions": "Be brief",
            "workspace": "  /tmp/codex  ", "scope": "channel", "other": 1,
        }));
        assert_eq!(
            saved,
            Config {
                model: "gpt".into(),
                effort: "high".into(),
                instructions: "Be brief".into(),
                workspace: "/tmp/codex".into(),
                scope: "channel".into(),
            }
        );
        for blank in [json!({ "workspace": "" }), json!({ "workspace": "  " })] {
            assert_eq!(Config::read(&blank).workspace, "~/.buzz");
        }
        assert_eq!(Config::read(&json!({ "scope": "dm" })).scope, "thread");
        assert_eq!(
            saved.value(),
            json!({
                "model": "gpt", "effort": "high", "instructions": "Be brief",
                "workspace": "/tmp/codex", "scope": "channel",
            })
        );
    }

    #[test]
    fn names_efforts_and_checks_workspaces() {
        assert_eq!(effort_name("xhigh"), "Extra high");
        assert_eq!(effort_name("max"), "Maximum");
        assert_eq!(effort_name("medium"), "Medium");
        assert_eq!(effort_name(""), "");
        assert!(valid_workspace("~/.buzz") && valid_workspace(" /tmp/x "));
        assert!(valid_workspace("C:\\work") && valid_workspace("d:/work"));
        assert!(valid_workspace("~/code") && valid_workspace(" ~ "));
        assert!(!valid_workspace("relative") && !valid_workspace("~other"));
    }
}
