//! Claude's device-wide defaults (buzz-app's `defaults.ts`), kept apart
//! from agent records: a blank agent value inherits them, and a blank
//! default leaves Claude Code in charge. buzz-app keeps them in
//! `localStorage["buzz.claude-code.provider-defaults.v1"]`; here they are a
//! JSON file of the same shape in the data directory.

use super::runtime::Config;
use crate::agents2::store::{FileStorage, Storage};
use serde_json::{Value, json};
use std::sync::Arc;

pub const STORAGE_KEY: &str = "buzz.claude-code.provider-defaults.v1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeProviderDefaults {
    pub model: String,
}

pub struct ClaudeDefaults {
    storage: Arc<dyn Storage>,
    state: ClaudeProviderDefaults,
}

impl ClaudeDefaults {
    /// The defaults saved in the app's data directory.
    pub fn load() -> Self {
        let path = crate::community::data_dir().join(format!("{STORAGE_KEY}.json"));
        Self::new(Arc::new(FileStorage(path)))
    }

    pub fn new(storage: Arc<dyn Storage>) -> Self {
        let state = read(storage.as_ref());
        Self { storage, state }
    }

    pub fn snapshot(&self) -> &ClaudeProviderDefaults {
        &self.state
    }

    /// The agent's config with a blank model inheriting the default.
    pub fn resolve(&self, raw: &Value) -> Value {
        let mut config = Config::read(raw);
        if config.model.trim().is_empty() {
            config.model = self.state.model.clone();
        }
        let mut value = raw.clone();
        match &mut value {
            Value::Object(fields) => {
                fields.insert("model".into(), Value::from(config.model));
            }
            _ => value = config.value(),
        }
        value
    }

    pub fn save(&mut self, next: ClaudeProviderDefaults) -> Result<(), String> {
        let text = json!({ "version": 1, "model": next.model }).to_string();
        self.storage
            .write(&text)
            .map_err(|_| "Claude's defaults could not be saved".to_string())?;
        self.state = next;
        Ok(())
    }
}

/// Malformed preferences keep Claude Code's native default.
fn read(storage: &dyn Storage) -> ClaudeProviderDefaults {
    storage
        .read()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|value| value.get("version") == Some(&Value::from(1)))
        .and_then(|value| value.get("model")?.as_str().map(str::to_owned))
        .map(|model| ClaudeProviderDefaults { model })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents2::store::MemoryStorage;
    use core::prelude::v1::test;

    #[test]
    fn a_blank_model_inherits_the_saved_default() {
        let storage = Arc::new(MemoryStorage::default());
        let mut defaults = ClaudeDefaults::new(storage.clone());
        let saved = json!({ "model": "", "workspace": "~/code", "extra": 1 });
        assert_eq!(
            defaults.resolve(&saved),
            saved,
            "no default, Claude decides"
        );
        defaults
            .save(ClaudeProviderDefaults {
                model: "opus".into(),
            })
            .unwrap();
        let resolved = defaults.resolve(&saved);
        assert_eq!(resolved["model"], "opus");
        assert_eq!(resolved["extra"], 1, "the record's other fields stay");
        let chosen = json!({ "model": "haiku" });
        assert_eq!(defaults.resolve(&chosen)["model"], "haiku");
        let reopened = ClaudeDefaults::new(storage.clone());
        assert_eq!(reopened.snapshot().model, "opus");
        assert_eq!(
            serde_json::from_str::<Value>(&storage.read().unwrap()).unwrap(),
            json!({ "version": 1, "model": "opus" })
        );
    }

    #[test]
    fn malformed_or_other_versions_keep_claudes_default() {
        for text in [
            "invalid",
            r#"{"version":2,"model":"opus"}"#,
            r#"{"version":1}"#,
        ] {
            let storage = Arc::new(MemoryStorage::default());
            storage.write(text).unwrap();
            assert_eq!(ClaudeDefaults::new(storage).snapshot().model, "", "{text}");
        }
    }
}
