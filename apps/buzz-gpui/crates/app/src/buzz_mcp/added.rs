//! Tools plugins add to every agent session (buzz-app's `agents.addTool`):
//! listed after the Buzz tools to Claude, Codex and every harness that
//! asks [`super::tools::tools`], and called through
//! [`super::tools::call_tool`]. A tool acts as the agent that calls it.
//!
//! Harnesses read their tool list when a session starts, so [`key`]
//! changes with the tools, and sessions started with other tools are
//! replaced: Claude through its fingerprint, Codex with a new thread.
//!
//! The registry is read from tool servers on the tokio runtime, so it is
//! a lock, not a GPUI global.

use super::tools::{Args, Reply};
use super::{BuzzClient, Context};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::RwLock;

/// Runs a tool for the agent behind `client`.
pub type Call = Arc<
    dyn Fn(Arc<dyn BuzzClient>, Context, Args) -> BoxFuture<'static, Result<Reply, String>>
        + Send
        + Sync,
>;

pub struct AddedTool {
    /// Who added it, such as a plugin id.
    pub owner: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub call: Call,
}

#[cfg(not(test))]
static ADDED: RwLock<Vec<Arc<AddedTool>>> = RwLock::new(Vec::new());

/// Runs `f` on the registry.
#[cfg(not(test))]
fn with<R>(f: impl FnOnce(&mut Vec<Arc<AddedTool>>) -> R) -> R {
    f(&mut ADDED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()))
}

// Tests run side by side and compare tool lists: each gets its own.
#[cfg(test)]
thread_local! {
    static ADDED: std::cell::RefCell<Vec<Arc<AddedTool>>> = std::cell::RefCell::default();
}

#[cfg(test)]
fn with<R>(f: impl FnOnce(&mut Vec<Arc<AddedTool>>) -> R) -> R {
    ADDED.with_borrow_mut(f)
}

/// buzz-app's rule: a lowercase letter, then up to 63 lowercase letters,
/// digits or `_`.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Adds `tool`, replacing its owner's tool of the same name. A Buzz tool's
/// name, or another owner's, is refused.
pub fn add(tool: AddedTool) -> Result<(), String> {
    if !valid_name(&tool.name) {
        return Err(format!(
            "“{}” is not a tool name. Use a lowercase letter, then lowercase letters, digits \
             or _.",
            tool.name
        ));
    }
    if super::tools::buzz_tool(&tool.name) || super::attention::is_attention_tool(&tool.name) {
        return Err(format!("“{}” is a Buzz tool.", tool.name));
    }
    if tool.description.trim().is_empty() {
        return Err(format!("“{}” needs a description.", tool.name));
    }
    if tool.input_schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!("“{}” needs an object inputSchema.", tool.name));
    }
    with(|added| {
        if added
            .iter()
            .any(|existing| existing.name == tool.name && existing.owner != tool.owner)
        {
            return Err(format!(
                "Another plugin adds the agent tool “{}”.",
                tool.name
            ));
        }
        added.retain(|existing| existing.name != tool.name);
        added.push(Arc::new(tool));
        Ok(())
    })
}

/// Removes `owner`'s tool `name`.
pub fn remove(owner: &str, name: &str) {
    with(|added| added.retain(|existing| !(existing.owner == owner && existing.name == name)));
}

pub fn get(name: &str) -> Option<Arc<AddedTool>> {
    with(|added| added.iter().find(|tool| tool.name == name).cloned())
}

/// The added tools as `tools/list` describes them, by name.
pub fn listed() -> Vec<Value> {
    let mut tools: Vec<Value> = with(|added| {
        added
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                })
            })
            .collect()
    });
    tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    tools
}

/// Changes whenever the added tools do (buzz-app's `addedKey`): empty
/// without any, else an FNV-1a hash of their listing.
pub fn key() -> String {
    let listed = listed();
    if listed.is_empty() {
        return String::new();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in Value::Array(listed).to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use futures::FutureExt;

    fn tool(owner: &str, name: &str) -> AddedTool {
        AddedTool {
            owner: owner.into(),
            name: name.into(),
            description: "Does a thing".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            call: Arc::new(|_, _, _| async { Ok(Reply::Text("done".into())) }.boxed()),
        }
    }

    #[test]
    fn tools_are_checked_listed_and_keyed() {
        let before = key();
        add(tool("a.plugin", "zz_added_test_lookup")).unwrap();
        assert!(get("zz_added_test_lookup").is_some());
        assert!(
            listed()
                .iter()
                .any(|tool| tool["name"] == "zz_added_test_lookup")
        );
        assert_ne!(key(), before);
        assert!(
            add(tool("b.plugin", "zz_added_test_lookup"))
                .unwrap_err()
                .contains("Another")
        );
        assert!(
            add(tool("a.plugin", "send"))
                .unwrap_err()
                .contains("Buzz tool")
        );
        assert!(
            add(tool("a.plugin", "Bad"))
                .unwrap_err()
                .contains("not a tool name")
        );
        let mut schemaless = tool("a.plugin", "zz_added_test_schema");
        schemaless.input_schema = json!({ "type": "string" });
        assert!(add(schemaless).unwrap_err().contains("object"));
        remove("a.plugin", "zz_added_test_lookup");
        assert!(get("zz_added_test_lookup").is_none());
    }

    #[test]
    fn names_follow_buzz_app() {
        assert!(valid_name("weather"));
        assert!(valid_name("get_forecast_2"));
        assert!(!valid_name("2fast"));
        assert!(!valid_name("get-forecast"));
        assert!(!valid_name(&"a".repeat(65)));
    }
}
