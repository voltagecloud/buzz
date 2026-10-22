//! Client for Buzz remote-agent provider binaries (`buzz-backend-*`).
//!
//! A provider speaks one JSON request on stdin and one JSON response on
//! stdout per process (`docs/remote-agents.md` §Provider Protocol). This crate
//! discovers, stages, and invokes providers, and runs the thread-bound
//! Agent Sandbox session lifecycle on top of them: start, end, and recover a
//! session under a per-agent lock, against a payload rebuilt after that lock
//! and checked against the tenant the caller captured.
//!
//! Hosts supply their agent records through [`InvocationSource`]. The
//! provider's own wire types stay in `crates/buzz-backend-kubernetes`; this
//! crate reads the same JSON without depending on it.
//!
//! The provider functions block. [`RemoteSessions`] runs them on Tokio's
//! blocking pool, so its methods need a Tokio runtime.

#![deny(unsafe_code)]

mod client;
mod error;
mod process;
mod provider;
mod recovery;
mod scope;
mod session;

pub use client::{AgentLocks, Invocation, InvocationSource, ProviderTarget, RemoteSessions};
pub use error::SessionError;
pub use process::spawn_with_busy_retry;
pub use provider::{
    invoke_provider, provider_deploy, provider_info, provider_stop, redact_env_values_in,
    redact_secrets_with, resolve_provider_binary, validate_provider_config, Discovery,
    ProviderInfo, StopState, PROTOCOL_VERSION,
};
pub use recovery::{
    offers_checkpoint, recovery_kind, RecoveryKind, RecoveryPrompt, RecoveryPromptKey,
    RecoveryPrompts, MAX_RECOVERY_PROMPTS,
};
pub use scope::{assert_expected_relay_scope, assert_expected_signer, relay_http_base_url};
pub use session::{
    is_thread_sandbox, RecoveryMode, SessionScope, Tenant, REPLAY_FLOOR_ENV_VAR, THREAD_PROVIDERS,
};
