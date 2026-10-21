//! Library surface of the Kubernetes provider: the provider protocol
//! operations and the Agent Sandbox session lifecycle, shared by the provider
//! binary and the in-cluster `buzz-sandbox-reaper` (reaper and session
//! manager).
//!
//! The provider binary ships inside the desktop app, so anything only a
//! consumer of this library needs (S3, HTTP servers, metrics, relay
//! connections) must live in that consumer, never here.

pub mod lifecycle;
pub mod provider;
pub mod wire;

mod classify;
mod client;
mod cluster;
mod config;
mod env;
mod gc;
mod identity;
mod image;
mod intent;
mod naming;
mod observe;
mod pod;
mod reconcile;
mod sandbox;

/// The in-memory apiserver the Sandbox tests drive, for consumers' tests.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub mod sandbox_fake;
