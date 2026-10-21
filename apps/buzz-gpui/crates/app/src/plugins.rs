//! Bundled plugins and their enable switches (`src/plugins`,
//! `crates/plugin-manager`).
//!
//! buzz-app loads its bundled plugins at run time; this port compiles them
//! in, so a plugin is the set of pages, panels, renderers, settings cards
//! and actions registered under its id. The enable state is saved by
//! buzz-app's own plugin manager, with its defaults: everything bundled is
//! on except Todos, Templates & teams and Bestie, and Channels cannot be
//! turned off. buzz-app's external `plugin.js` plugins do not load here.
//!
//! Contributions name their plugin with a `plugin/id` key; the registries
//! ask [`enabled_key`] before showing one, so turning a plugin off hides
//! everything it contributes without restarting.
//!
//! Plugins people build themselves are programs, not bundled code; see
//! [`crate::external_plugins`].

use buzzodz_plugins::{Catalog, Manager};
use gpui_kit::*;
use std::cell::RefCell;
use std::collections::HashSet;

/// The toast for an unconfirmed change.
const CHANGE_NOTICE: &str = "plugin-change";

/// The plugin that cannot be turned off.
pub const CHANNELS: &str = "buzz.channels";

/// One bundled plugin as Settings → Plugins lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRow {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

/// Why the saved plugin settings could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// No data directory: nothing can be saved.
    Storage(String),
    /// The saved settings could not be read (`RecoveryScreen`).
    Recovery(String),
}

pub struct Plugins {
    manager: Option<Manager>,
    rows: Vec<PluginRow>,
    unavailable: Option<Unavailable>,
    /// A change is being saved; the switches wait.
    busy: bool,
    /// The last change or recovery was not confirmed.
    error: Option<String>,
}

impl Global for Plugins {}

thread_local! {
    /// Disabled plugin ids, for registries that are read without an `App`.
    static DISABLED: RefCell<HashSet<String>> = RefCell::new(default_disabled());
}

/// buzz-app's defaults (`src/bundled/index.ts`), used only when the saved
/// settings cannot be read.
const DEFAULT_OFF: [&str; 2] = ["buzz.todos", "buzz.bestie"];

fn default_disabled() -> HashSet<String> {
    DEFAULT_OFF.iter().map(|id| (*id).to_owned()).collect()
}

fn rows(catalog: Catalog) -> Vec<PluginRow> {
    catalog
        .plugins
        .into_iter()
        // Only bundled plugins exist here; an external entry saved by
        // buzz-app never loads.
        .filter(|plugin| plugin.source == "bundled")
        .map(|plugin| PluginRow {
            enabled: plugin.enabled,
            id: plugin.manifest.id,
            name: plugin.manifest.name,
        })
        .collect()
}

/// Where plugin settings live: `BUZZODZ_HOME`, else `plugins` in this
/// app's data directory.
pub fn home() -> std::path::PathBuf {
    std::env::var_os("BUZZODZ_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::community::data_dir().join("plugins"))
}

/// Opens the plugin settings under this app's data directory.
fn open() -> Result<Manager, String> {
    Manager::open(Some(home()), "default", false)
}

impl Plugins {
    pub fn init(cx: &mut App) {
        let state = match open() {
            Ok(manager) => match manager.catalog() {
                Ok(catalog) => Self {
                    rows: rows(catalog),
                    manager: Some(manager),
                    unavailable: None,
                    busy: false,
                    error: None,
                },
                Err(reason) => {
                    tracing::warn!(%reason, "plugin settings unreadable; using defaults");
                    Self {
                        manager: Some(manager),
                        rows: Vec::new(),
                        unavailable: Some(Unavailable::Recovery(reason)),
                        busy: false,
                        error: None,
                    }
                }
            },
            Err(reason) => {
                tracing::warn!(%reason, "plugin settings unavailable; using defaults");
                Self {
                    manager: None,
                    rows: Vec::new(),
                    unavailable: Some(Unavailable::Storage(reason)),
                    busy: false,
                    error: None,
                }
            }
        };
        state.mirror();
        cx.set_global(state);
    }

    pub fn rows(&self) -> &[PluginRow] {
        &self.rows
    }

    pub fn unavailable(&self) -> Option<&Unavailable> {
        self.unavailable.as_ref()
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn mirror(&self) {
        let disabled = if self.unavailable.is_some() || self.rows.is_empty() {
            default_disabled()
        } else {
            self.rows
                .iter()
                .filter(|row| !row.enabled && row.id != CHANNELS)
                .map(|row| row.id.clone())
                .collect()
        };
        DISABLED.with_borrow_mut(|current| *current = disabled);
    }

    /// Turns a bundled plugin on or off and saves it.
    pub fn change(id: &str, enable: bool, cx: &mut App) {
        let state = cx.global_mut::<Self>();
        let Some(manager) = state.manager.clone() else {
            return;
        };
        if state.busy || (id == CHANNELS && !enable) {
            return;
        }
        state.busy = true;
        state.error = None;
        let action = if enable { "enable" } else { "disable" };
        let id = id.to_owned();
        cx.refresh_windows();
        let task = cx
            .background_executor()
            .spawn(async move { manager.change(action, &id) });
        cx.spawn(async move |cx| {
            let result = task.await;
            cx.update(|cx| Self::settled(result, cx));
        })
        .detach();
    }

    /// Backs up the unreadable settings and restores the defaults.
    pub fn recover(cx: &mut App) {
        let state = cx.global_mut::<Self>();
        let Some(manager) = state.manager.clone() else {
            return;
        };
        if state.busy {
            return;
        }
        state.busy = true;
        state.error = None;
        cx.refresh_windows();
        let task = cx
            .background_executor()
            .spawn(async move { manager.recover() });
        cx.spawn(async move |cx| {
            let result = task.await;
            cx.update(|cx| Self::settled(result, cx));
        })
        .detach();
    }

    /// Reads the saved settings again (`Try again`).
    pub fn retry(cx: &mut App) {
        let state = cx.global_mut::<Self>();
        let Some(manager) = state.manager.clone() else {
            return;
        };
        if state.busy {
            return;
        }
        state.busy = true;
        cx.refresh_windows();
        let task = cx
            .background_executor()
            .spawn(async move { manager.catalog() });
        cx.spawn(async move |cx| {
            let result = task.await;
            cx.update(|cx| Self::settled(result, cx));
        })
        .detach();
    }

    fn settled(result: Result<Catalog, String>, cx: &mut App) {
        let state = cx.global_mut::<Self>();
        state.busy = false;
        let failed = match result {
            Ok(catalog) => {
                state.rows = rows(catalog);
                state.unavailable = None;
                None
            }
            Err(reason) => {
                state.error = Some(reason.clone());
                (state.unavailable.is_none()).then_some(reason)
            }
        };
        state.mirror();
        crate::external_plugins::ExternalPlugins::sync_bundled(cx);
        if let Some(reason) = failed {
            use crate::ui::toasts::{self, Notice};
            toasts::show(
                Notice::error(CHANGE_NOTICE, "Plugin change wasn’t confirmed").description(
                    format!("Check the current settings before trying again. {reason}"),
                ),
                cx,
            );
        } else {
            crate::ui::toasts::dismiss(CHANGE_NOTICE, cx);
        }
        cx.refresh_windows();
    }
}

/// Whether plugin `id` is on. Anything that is not a bundled plugin id,
/// such as a shell-owned key, counts as on.
pub fn enabled(id: &str) -> bool {
    id == CHANNELS || !DISABLED.with_borrow(|disabled| disabled.contains(id))
}

/// The plugin a `plugin/id` contribution key belongs to.
pub fn plugin_of(key: &str) -> &str {
    key.split_once('/').map_or(key, |(plugin, _)| plugin)
}

/// Whether the plugin that owns contribution `key` is on.
pub fn enabled_key(key: &str) -> bool {
    enabled(plugin_of(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn saved_switches_follow_buzz_app_defaults_and_keep_channels_on() {
        let home = tempfile_dir();
        let manager = Manager::open(Some(home.clone()), "default", false).unwrap();
        let catalog = rows(manager.catalog().unwrap());
        let on = |id: &str| catalog.iter().find(|row| row.id == id).unwrap().enabled;
        assert!(on("buzz.inbox") && on("buzz.reminders") && on("buzz.identity-naming"));
        assert!(!on("buzz.todos") && !on("buzz.bestie"));
        let changed = rows(manager.change("enable", "buzz.todos").unwrap());
        assert!(
            changed
                .iter()
                .any(|row| row.id == "buzz.todos" && row.enabled)
        );
        assert!(manager.change("disable", CHANNELS).is_err());
        // The switch is saved in buzz-app's registry format.
        let saved = std::fs::read_to_string(home.join("profiles/default/registry.json")).unwrap();
        assert!(saved.contains("\"buzz.todos\": true"), "{saved}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_agents2_switch_becomes_the_agents_switch() {
        // A registry from before the rename: v1 Agents on, Agents2 off.
        let home = tempfile_dir();
        let profile = home.join("profiles/default");
        std::fs::create_dir_all(&profile).unwrap();
        let registry = serde_json::json!({
            "version": 1,
            "bundledEnabled": true,
            "bundledOverrides": { "buzz.agents": true, "buzz.agents2": false },
            "installed": {},
        });
        std::fs::write(profile.join("registry.json"), registry.to_string()).unwrap();
        let manager = Manager::open(Some(home.clone()), "default", false).unwrap();
        let catalog = rows(manager.catalog().unwrap());
        let agents = catalog.iter().find(|row| row.id == "buzz.agents").unwrap();
        assert_eq!((agents.name.as_str(), agents.enabled), ("Agents", false));
        assert!(!catalog.iter().any(|row| row.id == "buzz.agents2"));
        // Once moved, a later change sticks.
        manager.change("enable", "buzz.agents").unwrap();
        let catalog = rows(manager.catalog().unwrap());
        assert!(
            catalog
                .iter()
                .any(|row| row.id == "buzz.agents" && row.enabled)
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn agent_types_are_bundled_and_on_and_keep_an_installed_copys_switch() {
        let home = tempfile_dir();
        let manager = Manager::open(Some(home.clone()), "default", false).unwrap();
        let catalog = rows(manager.catalog().unwrap());
        let row = |rows: &[PluginRow], id: &str| rows.iter().find(|row| row.id == id).cloned();
        assert_eq!(
            row(&catalog, "buzz.claude-code").map(|row| (row.name, row.enabled)),
            Some(("Claude".into(), true))
        );
        assert_eq!(
            row(&catalog, "buzz.codex").map(|row| (row.name, row.enabled)),
            Some(("Codex".into(), true))
        );
        let _ = std::fs::remove_dir_all(home);

        // A profile that installed Codex from a folder and turned it off
        // gets the bundled copy, still off.
        let home = tempfile_dir();
        let profile = home.join("profiles/default");
        std::fs::create_dir_all(&profile).unwrap();
        let registry = serde_json::json!({
            "version": 1,
            "bundledEnabled": true,
            "bundledOverrides": {},
            "installed": {
                "buzz.codex": {
                    "manifest": { "id": "buzz.codex", "name": "Codex", "apiVersion": 1 },
                    "current": "0".repeat(64),
                    "previous": null,
                    "enabled": false,
                },
            },
        });
        std::fs::write(profile.join("registry.json"), registry.to_string()).unwrap();
        let manager = Manager::open(Some(home.clone()), "default", false).unwrap();
        let catalog = rows(manager.catalog().unwrap());
        assert_eq!(
            row(&catalog, "buzz.codex").map(|row| row.enabled),
            Some(false)
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn keys_name_their_plugin() {
        assert_eq!(plugin_of("buzz.inbox/inbox"), "buzz.inbox");
        assert_eq!(plugin_of("settings"), "settings");
        DISABLED.with_borrow_mut(|disabled| *disabled = default_disabled());
        assert!(!enabled_key("buzz.todos/todos"));
        assert!(enabled_key("buzz.channels/channels"));
        assert!(enabled_key("settings"));
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("buzz-plugins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
