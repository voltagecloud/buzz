//! The thread-session lifecycle over a host's agent records.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::provider::{
    provider_deploy, provider_info, provider_stop, resolve_provider_binary, Discovery, StopState,
};
use crate::session::{
    apply_replay_floor, assert_payload_scope, bind_sandbox_scope, is_thread_sandbox, Action,
    RecoveryMode, SessionScope, Tenant,
};
use crate::SessionError;

/// The provider an agent runs on, read from its current record.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderTarget {
    pub provider_id: String,
    /// The record's public provider configuration. Invocations bind the
    /// session into a copy; this value is never changed.
    pub provider_config: serde_json::Value,
    /// The binary path the host cached when the agent was created. It is
    /// used only if it is still a discovered candidate for `provider_id`.
    pub cached_binary: Option<PathBuf>,
}

/// One provider invocation, rebuilt from the current record.
#[derive(Clone, Debug, PartialEq)]
pub struct Invocation {
    pub target: ProviderTarget,
    /// The deploy payload (`agent` on the wire), built from live host state:
    /// its `relay_url` and `launch.owner_pubkey` are asserted against the
    /// caller's [`Tenant`].
    pub payload: serde_json::Value,
}

/// The host's agent records, as [`RemoteSessions`] needs them.
///
/// This is the seam between the session lifecycle and a host's storage:
/// the desktop reads its managed-agent store; another host supplies its own.
pub trait InvocationSource: Send + Sync {
    /// The agent's provider, or `None` when the agent does not run on one.
    fn target(&self, agent_pubkey: &str) -> Result<Option<ProviderTarget>, SessionError>;

    /// Rebuild the invocation from the current record. Called only while
    /// the agent's lock is held: the call may have waited behind another
    /// operation, so the invocation must carry the newest saved policy and
    /// the live tenant, not a snapshot its caller captured. Fails when the
    /// agent does not run on a provider.
    fn rebuild(&self, agent_pubkey: &str) -> Result<Invocation, SessionError>;

    /// Record a deploy's outcome — the backend agent id or the failure —
    /// against the payload that was deployed. Called under the agent's lock,
    /// before the outcome is returned; an error here is returned instead.
    fn record_deploy(
        &self,
        _agent_pubkey: &str,
        _result: &Result<String, SessionError>,
        _payload: &serde_json::Value,
    ) -> Result<(), SessionError> {
        Ok(())
    }
}

/// Per-agent provider-operation locks. Clones share the same locks; a host
/// keeps one instance for its lifetime.
#[derive(Clone, Debug, Default)]
pub struct AgentLocks(Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>);

impl AgentLocks {
    /// Serialize provider operations per agent. The guard must stay alive
    /// until the provider invocation finishes.
    ///
    /// One lock per agent, not per thread: it serializes the record rebuild,
    /// provider call, and result write. A thread Sandbox cold start holds it
    /// through the provider's Pod waits (120s bound each), so starts for the
    /// same agent's other threads queue behind it.
    async fn lock(
        &self,
        agent_pubkey: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, SessionError> {
        let lock = {
            let mut locks = self.0.lock().map_err(|error| error.to_string())?;
            Arc::clone(
                locks
                    .entry(agent_pubkey.to_string())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        Ok(lock.lock_owned().await)
    }

    /// Whether an operation holds `agent_pubkey`'s lock. Only the Unix
    /// tests use it, so it is compiled with them.
    #[cfg(all(test, unix))]
    pub(crate) fn is_held(&self, agent_pubkey: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(agent_pubkey)
            .is_some_and(|lock| lock.try_lock().is_err())
    }
}

/// Start, end, and recover agents' remote sessions.
///
/// Every operation takes the agent's lock, rebuilds its invocation through
/// the [`InvocationSource`], asserts the caller's tenant against that
/// rebuilt payload, and binds the thread session before invoking the
/// provider. Methods need a Tokio runtime: provider calls run on its blocking
/// pool.
pub struct RemoteSessions<S> {
    source: S,
    locks: AgentLocks,
    discovery: Discovery,
    workdir: Option<PathBuf>,
}

/// A provider call ready to run.
struct Prepared {
    binary: PathBuf,
    config: serde_json::Value,
    payload: serde_json::Value,
}

impl<S: InvocationSource> RemoteSessions<S> {
    /// Providers are found through `discovery` and run in `workdir`.
    pub fn new(
        source: S,
        locks: AgentLocks,
        discovery: Discovery,
        workdir: Option<PathBuf>,
    ) -> Self {
        Self {
            source,
            locks,
            discovery,
            workdir,
        }
    }

    /// Deploy (start or update) an agent, then record the outcome.
    ///
    /// Idempotency: deploying an already-deployed agent sends the same
    /// payload again; providers treat it as an update-in-place or no-op.
    /// A thread-sandbox agent needs `session`; other agents ignore it.
    ///
    /// `replay_floor_unix`: optional unix-seconds replay floor from a
    /// publish-first mention send, injected as [`REPLAY_FLOOR_ENV_VAR`] so the
    /// remote harness replays back past the already-published triggering
    /// message exactly like a local spawn. Per-invocation only — never
    /// recorded, so later redeploys do not carry a stale floor.
    ///
    /// [`REPLAY_FLOOR_ENV_VAR`]: crate::REPLAY_FLOOR_ENV_VAR
    pub async fn deploy(
        &self,
        agent_pubkey: &str,
        tenant: Tenant<'_>,
        session: Option<&SessionScope>,
        replay_floor_unix: Option<u64>,
    ) -> Result<(), SessionError> {
        self.run_deploy(
            agent_pubkey,
            tenant,
            session,
            Action::Deploy,
            replay_floor_unix,
        )
        .await
    }

    /// End the thread's session with the provider's `stop`. The agent record
    /// is not changed: the provider owns the session lifecycle, and recovery
    /// is a separate explicit action.
    pub async fn end(
        &self,
        agent_pubkey: &str,
        tenant: Tenant<'_>,
        session: &SessionScope,
    ) -> Result<StopState, SessionError> {
        let tenant = tenant.require_both()?;
        let _guard = self.locks.lock(agent_pubkey).await?;
        let Prepared {
            binary,
            config,
            payload,
        } = self.prepare(agent_pubkey, tenant, Some(session), Action::End)?;
        let workdir = self.workdir.clone();
        tokio::task::spawn_blocking(move || {
            provider_stop(&binary, workdir.as_deref(), &payload, &config)
        })
        .await
        .map_err(|e| format!("spawn_blocking failed: {e}"))?
    }

    /// Recover the thread's ended session from its checkpoint or fresh. The
    /// mode is sent for this deploy only and is never recorded.
    pub async fn recover(
        &self,
        agent_pubkey: &str,
        tenant: Tenant<'_>,
        session: &SessionScope,
        mode: RecoveryMode,
    ) -> Result<(), SessionError> {
        let tenant = tenant.require_both()?;
        self.run_deploy(
            agent_pubkey,
            tenant,
            Some(session),
            Action::Recover(mode),
            None,
        )
        .await
    }

    /// Whether the agent's provider can end and recover thread sessions.
    /// Runs only `info`. Agents without thread workspaces return `false`.
    pub async fn supports_lifecycle(&self, agent_pubkey: &str) -> Result<bool, SessionError> {
        let Some(target) = self
            .source
            .target(agent_pubkey)?
            .filter(|target| is_thread_sandbox(&target.provider_id, &target.provider_config))
        else {
            return Ok(false);
        };
        let binary = resolve_provider_binary(
            &target.provider_id,
            target.cached_binary.as_deref(),
            &self.discovery,
        )?;
        let workdir = self.workdir.clone();
        let info = tokio::task::spawn_blocking(move || provider_info(&binary, workdir.as_deref()))
            .await
            .map_err(|e| format!("spawn_blocking failed: {e}"))??;
        Ok(info.supports_lifecycle())
    }

    async fn run_deploy(
        &self,
        agent_pubkey: &str,
        tenant: Tenant<'_>,
        session: Option<&SessionScope>,
        action: Action,
        replay_floor_unix: Option<u64>,
    ) -> Result<(), SessionError> {
        let _guard = self.locks.lock(agent_pubkey).await?;
        let Prepared {
            binary,
            config,
            mut payload,
        } = self.prepare(agent_pubkey, tenant, session, action)?;
        // The floor is invocation state, not record state, so the post-lock
        // rebuild cannot restore it — inject it into the payload invoked.
        apply_replay_floor(&mut payload, replay_floor_unix);
        let workdir = self.workdir.clone();
        let (result, payload) = tokio::task::spawn_blocking(move || {
            let result = provider_deploy(&binary, workdir.as_deref(), &payload, &config);
            (result, payload)
        })
        .await
        .map_err(|e| format!("spawn_blocking failed: {e}"))?;
        self.source.record_deploy(agent_pubkey, &result, &payload)?;
        result.map(drop)
    }

    /// Rebuild the invocation under the agent's lock, assert the caller's
    /// tenant against it, and bind the session into a copy of the config.
    fn prepare(
        &self,
        agent_pubkey: &str,
        tenant: Tenant<'_>,
        session: Option<&SessionScope>,
        action: Action,
    ) -> Result<Prepared, SessionError> {
        let Invocation { target, payload } = self.source.rebuild(agent_pubkey)?;
        // The rebuild re-read the live relay and owner identity. Assert the
        // caller's captured tenant against THIS payload — the exact value
        // invoked — not a pre-lock snapshot its caller validated.
        assert_payload_scope(&payload, tenant, action)?;
        // `config` is a copy: binding the session and any recovery mode
        // never reaches the record's saved `sandbox: true`.
        let mut config = target.provider_config;
        if is_thread_sandbox(&target.provider_id, &config) {
            bind_sandbox_scope(
                &mut config,
                session,
                agent_pubkey,
                &payload,
                action.recovery(),
            )?;
        } else if action != Action::Deploy {
            return Err("Only thread workspaces can be ended or recovered".into());
        }
        let binary = resolve_provider_binary(
            &target.provider_id,
            target.cached_binary.as_deref(),
            &self.discovery,
        )?;
        Ok(Prepared {
            binary,
            config,
            payload,
        })
    }
}

#[cfg(all(test, unix))]
#[path = "client_tests.rs"]
mod tests;
