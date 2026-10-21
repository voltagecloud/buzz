//! Codex's device-wide defaults and how agents inherit them (buzz-app's
//! `CodexProvider` default store and `resolve`). buzz-app keeps them in
//! `localStorage["buzz.codex.provider-defaults.v1"]`; here they are a JSON
//! file of the same shape in the data directory.

use super::config::{Config, Model, effort_name};
use crate::agents2::store::{FileStorage, Storage};
use serde_json::{Value, json};
use std::sync::Arc;

pub const STORAGE_KEY: &str = "buzz.codex.provider-defaults.v1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexProviderDefaults {
    pub model: String,
    pub effort: String,
}

pub struct CodexDefaults {
    storage: Arc<dyn Storage>,
    state: CodexProviderDefaults,
}

/// The catalog's model of this id, or its default model for none.
pub fn find<'a>(models: Option<&'a [Model]>, model: &str) -> Option<&'a Model> {
    models?.iter().find(|candidate| {
        if model.is_empty() {
            candidate.is_default
        } else {
            candidate.model == model
        }
    })
}

fn supports(model: Option<&Model>, effort: &str) -> bool {
    model.is_some_and(|model| {
        model
            .supported_reasoning_efforts
            .iter()
            .any(|choice| choice.reasoning_effort == effort)
    })
}

impl CodexDefaults {
    /// The defaults saved in the app's data directory.
    pub fn load() -> Self {
        let path = crate::community::data_dir().join(format!("{STORAGE_KEY}.json"));
        Self::new(Arc::new(FileStorage(path)))
    }

    pub fn new(storage: Arc<dyn Storage>) -> Self {
        let state = read(storage.as_ref());
        Self { storage, state }
    }

    pub fn snapshot(&self) -> &CodexProviderDefaults {
        &self.state
    }

    /// Saves `next`, keeping its effort only where its model offers it.
    pub fn save(
        &mut self,
        next: CodexProviderDefaults,
        models: Option<&[Model]>,
    ) -> Result<(), String> {
        let selected = find(models, &next.model);
        let effort = if next.effort.is_empty() || supports(selected, &next.effort) {
            next.effort
        } else {
            selected
                .map(|model| model.default_reasoning_effort.clone())
                .unwrap_or_default()
        };
        let text = json!({ "version": 1, "model": next.model, "effort": effort }).to_string();
        self.storage
            .write(&text)
            .map_err(|_| "Codex's defaults could not be saved".to_string())?;
        self.state = CodexProviderDefaults {
            model: next.model,
            effort,
        };
        Ok(())
    }

    /// The agent's config with blank choices inheriting the defaults. A
    /// default model the catalog no longer has falls back to Codex's own,
    /// and an inherited effort the model lacks to the model's default.
    pub fn resolve(&self, explicit: Config, models: Option<&[Model]>) -> Config {
        let inherited = &self.state.model;
        let unavailable =
            !inherited.is_empty() && models.is_some() && find(models, inherited).is_none();
        let model = if !explicit.model.is_empty() {
            explicit.model.clone()
        } else if unavailable {
            String::new()
        } else {
            inherited.clone()
        };
        if !explicit.effort.is_empty() {
            return Config { model, ..explicit };
        }
        let selected = find(models, &model);
        let preferred = if unavailable && explicit.model.is_empty() {
            ""
        } else {
            self.state.effort.as_str()
        };
        let effort = if preferred.is_empty() {
            String::new()
        } else if models.is_none() || supports(selected, preferred) {
            preferred.to_owned()
        } else {
            selected
                .map(|model| model.default_reasoning_effort.clone())
                .unwrap_or_default()
        };
        Config {
            model,
            effort,
            ..explicit
        }
    }
}

/// Malformed preferences keep Codex's native defaults.
fn read(storage: &dyn Storage) -> CodexProviderDefaults {
    let value = storage
        .read()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|value| value.get("version") == Some(&Value::from(1)));
    let field = |name: &str| {
        value
            .as_ref()
            .and_then(|value| value.get(name)?.as_str().map(str::to_owned))
    };
    match (field("model"), field("effort")) {
        (Some(model), Some(effort)) => CodexProviderDefaults { model, effort },
        _ => CodexProviderDefaults::default(),
    }
}

/// The model menu's default row: the device default when one is saved
/// and the catalog still has it.
pub fn provider_default_label(model: &str, models: Option<&[Model]>) -> String {
    if !model.is_empty() {
        let selected = find(models, model);
        if models.is_none() || selected.is_some() {
            let name = selected
                .map(|selected| {
                    if selected.display_name.is_empty() {
                        selected.model.clone()
                    } else {
                        selected.display_name.clone()
                    }
                })
                .unwrap_or_else(|| model.to_owned());
            return format!("Provider default · {name}");
        }
    }
    native_label(models)
}

/// "Codex default", with the model Codex recommends once it says.
pub fn native_label(models: Option<&[Model]>) -> String {
    match find(models, "") {
        Some(native) if !native.display_name.is_empty() => {
            format!("Codex default ({})", native.display_name)
        }
        Some(native) if !native.model.is_empty() => format!("Codex default ({})", native.model),
        _ => "Codex default".into(),
    }
}

/// The thinking select's default row, naming the effort it comes to.
pub fn default_effort_label(effort: &str, model: Option<&Model>) -> String {
    let resolved = if effort.is_empty() {
        model.map_or("", |model| model.default_reasoning_effort.as_str())
    } else {
        effort
    };
    if resolved.is_empty() {
        "Codex default".into()
    } else {
        format!("Codex default ({})", effort_name(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_plugins::codex::config::Effort;
    use crate::agents2::store::MemoryStorage;
    use core::prelude::v1::test;

    fn catalog() -> Vec<Model> {
        let effort = |id: &str| Effort {
            reasoning_effort: id.into(),
            description: String::new(),
        };
        vec![
            Model {
                model: "model-a".into(),
                display_name: "Model A".into(),
                is_default: true,
                default_reasoning_effort: "low".into(),
                supported_reasoning_efforts: vec![effort("low"), effort("high")],
                ..Default::default()
            },
            Model {
                model: "model-b".into(),
                display_name: "Model B".into(),
                default_reasoning_effort: "medium".into(),
                supported_reasoning_efforts: vec![effort("low"), effort("medium")],
                ..Default::default()
            },
        ]
    }

    fn defaults() -> (Arc<MemoryStorage>, CodexDefaults) {
        let storage = Arc::new(MemoryStorage::default());
        (storage.clone(), CodexDefaults::new(storage))
    }

    #[test]
    fn provider_precedence_falls_back_for_incompatible_thinking() {
        let models = catalog();
        let models = Some(models.as_slice());
        let (_, mut store) = defaults();
        let config = Config::default();
        assert_eq!(store.resolve(config.clone(), models), config);
        store
            .save(
                CodexProviderDefaults {
                    model: "model-a".into(),
                    effort: "high".into(),
                },
                models,
            )
            .unwrap();
        let inherited = store.resolve(config.clone(), models);
        assert_eq!(
            (inherited.model.as_str(), inherited.effort.as_str()),
            ("model-a", "high")
        );
        let b = Config {
            model: "model-b".into(),
            ..config.clone()
        };
        // Model B has no high effort, so it takes its own default.
        let resolved = store.resolve(b.clone(), models);
        assert_eq!(
            (resolved.model.as_str(), resolved.effort.as_str()),
            ("model-b", "medium")
        );
        let explicit = Config {
            effort: "high".into(),
            ..b
        };
        assert_eq!(store.resolve(explicit.clone(), models), explicit);
    }

    #[test]
    fn an_unavailable_default_falls_back_until_the_catalog_has_it() {
        let (storage, _) = defaults();
        storage
            .write(r#"{"version":1,"model":"model-a","effort":"high"}"#)
            .unwrap();
        let store = CodexDefaults::new(storage.clone());
        let config = Config::default();
        // Before the catalog loads, the defaults apply as saved.
        let early = store.resolve(config.clone(), None);
        assert_eq!(
            (early.model.as_str(), early.effort.as_str()),
            ("model-a", "high")
        );
        let without_a: Vec<Model> = catalog().into_iter().skip(1).collect();
        assert_eq!(store.resolve(config.clone(), Some(&without_a)), config);
        let b = Config {
            model: "model-b".into(),
            ..config.clone()
        };
        assert_eq!(store.resolve(b, Some(&without_a)).effort, "medium");
        assert!(
            storage.read().unwrap().contains("model-a"),
            "nothing rewritten"
        );
        let models = catalog();
        let recovered = store.resolve(config, Some(&models));
        assert_eq!(
            (recovered.model.as_str(), recovered.effort.as_str()),
            ("model-a", "high")
        );
    }

    #[test]
    fn saving_keeps_only_an_effort_the_model_offers() {
        let models = catalog();
        let models = Some(models.as_slice());
        let (storage, mut store) = defaults();
        store
            .save(
                CodexProviderDefaults {
                    model: "model-b".into(),
                    effort: "high".into(),
                },
                models,
            )
            .unwrap();
        assert_eq!(store.snapshot().effort, "medium");
        store
            .save(
                CodexProviderDefaults {
                    model: String::new(),
                    effort: String::new(),
                },
                models,
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&storage.read().unwrap()).unwrap(),
            json!({ "version": 1, "model": "", "effort": "" })
        );
        for text in ["invalid", r#"{"version":1,"model":"model-a"}"#] {
            storage.write(text).unwrap();
            assert_eq!(
                CodexDefaults::new(storage.clone()).snapshot(),
                &CodexProviderDefaults::default()
            );
        }
    }

    #[test]
    fn labels_name_the_device_default_or_codexs_own() {
        let models = catalog();
        let models = Some(models.as_slice());
        assert_eq!(provider_default_label("", None), "Codex default");
        assert_eq!(
            provider_default_label("", models),
            "Codex default (Model A)"
        );
        assert_eq!(
            provider_default_label("model-b", models),
            "Provider default · Model B"
        );
        assert_eq!(
            provider_default_label("gone", models),
            "Codex default (Model A)",
            "a default Codex no longer has"
        );
        assert_eq!(
            provider_default_label("gpt", None),
            "Provider default · gpt"
        );
        assert_eq!(
            default_effort_label("", find(models, "")),
            "Codex default (Low)"
        );
        assert_eq!(
            default_effort_label("xhigh", None),
            "Codex default (Extra high)"
        );
        assert_eq!(default_effort_label("", None), "Codex default");
    }
}
