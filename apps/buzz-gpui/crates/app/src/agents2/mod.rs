//! Agents2: agents that are types from the start (buzz-app's
//! `features/agents2`). A compiled-in agent type ([`AgentType`]) gives its
//! summary line, peek view, settings tabs and `run`. Each agent made from
//! it has its own key held by an agent host (`buzz_host::api::AppAgentRequest`),
//! an `agent-attention/v1` configuration the app owns and edits, and a
//! config value the type owns. The app delivers matching live events to
//! `run` from the stream the owner already receives in the agent's
//! community, selected or not, so no type opens a socket of its own.
//!
//! - [`registry`]: the [`AgentType`] trait and [`register_type`].
//! - [`runtime`]: what a run receives ([`Delivery`], [`AgentHandle`]).
//! - [`attention`] and [`store`]: the attention format and local records.
//! - [`objects`]: the agent's own one-object reads and writes of it.
//! - [`Agents2`]: the service, a GPUI entity reachable with
//!   [`Agents2::global`].

pub mod attention;
pub(crate) mod cleanup;
#[cfg(debug_assertions)]
mod dev;
pub mod listings;
pub mod objects;
mod presence;
pub mod registry;
#[cfg(feature = "voltage-sandbox")]
pub mod remote_migration;
pub mod runtime;
mod service;
pub mod store;
pub mod teams;
mod typing;
pub(crate) mod working;

use attention::{AttentionObject, TimerState};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use store::SkippedObject;

#[allow(unused_imports)]
pub use registry::{
    AgentConversation, AgentProvider, AgentTab, AgentType, AgentView, CarryKey, CloseProvider,
    CreateModels, Effort, MigratedSetting, MigrationOutcome, MigrationSetting, Model, ModelChoice,
    Models, ProviderChanges, ProviderSnapshot, Readiness, RegisteredType, RetypePreview,
    SettingsHost, TypeDefaults, providers_changed, register_type, retype_plan, type_of, types,
};
#[allow(unused_imports)]
pub use runtime::{AgentHandle, Cancellation, Delivery, Trigger};
pub use service::{Agents2, PICTURES_UNSUPPORTED, Portable, Portables, Status, is_hosted};

/// One of the viewer's agents, in the community at `relay`.
#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub pubkey: String,
    pub name: String,
    /// The agent type's key, `<plugin>/<id>`.
    pub agent_type: String,
    pub owner: String,
    /// The canonical `wss://` community origin it was created for.
    pub relay: String,
    /// The picture its host saved for its profile; empty for none. `None`
    /// when its host is from before pictures, so only its community's
    /// profile knows it.
    pub picture: Option<String>,
    pub attention: Arc<BTreeMap<String, AttentionObject>>,
    /// Stored attention objects that are invalid or over a limit, by slug.
    pub skipped: Arc<BTreeMap<String, SkippedObject>>,
    /// Run state of its timers, by slug; written only by the runtime.
    pub timers: Arc<BTreeMap<String, TimerState>>,
    /// Whether its watches and timers wake it. Off keeps every object.
    pub attention_enabled: bool,
    pub config: Value,
}

/// What a new agent is made with besides its type and name.
#[derive(Clone, Debug, Default)]
pub struct Made {
    /// Replaces the type's starting config.
    pub config: Option<Value>,
    /// An uploaded or pack picture's URL.
    pub picture: Option<String>,
    /// What it carries from where it came from, for its type to adopt.
    pub carry: Option<buzz_host::api::Carry>,
}

/// A change to an agent. `attention` maps slugs to new values; `None`
/// deletes the object.
#[derive(Clone, Debug, Default)]
pub struct AgentChange {
    pub name: Option<String>,
    /// An uploaded picture's URL; empty removes it.
    pub picture: Option<String>,
    pub config: Option<Value>,
    pub attention: Vec<(String, Option<Value>)>,
    pub attention_enabled: Option<bool>,
    /// Timers in `attention` to start over: a new schedule from their
    /// `armed_at`, with no occurrences used.
    pub restart: Vec<String>,
}

/// Starts the Agents2 service. Call after the hosts and the app state.
/// Agent types register before or after this, from their own `init`.
pub fn init(cx: &mut gpui_kit::App) {
    #[cfg(debug_assertions)]
    dev::init(cx);
    service::init(cx);
}
