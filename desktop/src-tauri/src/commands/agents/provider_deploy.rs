//! The desktop's agent records as the remote-session client reads them, and
//! the deploy entry point every start path shares.

use std::path::PathBuf;

use buzz_remote_sessions::{
    Invocation, InvocationSource, ProviderTarget, RemoteSessions, SessionError, SessionScope,
    Tenant,
};
use tauri::AppHandle;

use crate::{
    app_state::AppState,
    managed_agents::{
        default_agent_workdir, load_managed_agents, provider_discovery, save_managed_agents,
        BackendKind, ManagedAgentRecord, ProviderFailure,
    },
    util::now_iso,
};

use super::build_deploy_payload;

/// The managed-agent store behind [`RemoteSessions`]. Every read and write
/// takes the store lock; the session client already holds the agent's
/// provider-operation lock when it rebuilds or records.
pub(super) struct DesktopAgents<'a, R: tauri::Runtime> {
    app: &'a AppHandle<R>,
    state: &'a AppState,
}

/// The remote-session client over the desktop's records, sharing the
/// app-lifetime per-agent locks. Providers are discovered per operation and
/// run in the agent working directory.
pub(super) fn remote_sessions<'a, R: tauri::Runtime>(
    app: &'a AppHandle<R>,
    state: &'a AppState,
) -> RemoteSessions<DesktopAgents<'a, R>> {
    RemoteSessions::new(
        DesktopAgents { app, state },
        state.provider_deploy_locks.clone(),
        provider_discovery(),
        default_agent_workdir(),
    )
}

impl<R: tauri::Runtime> DesktopAgents<'_, R> {
    fn with_record<T>(
        &self,
        pubkey: &str,
        read: impl FnOnce(&ManagedAgentRecord) -> Result<T, String>,
    ) -> Result<T, SessionError> {
        let _store_guard = self
            .state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let records = load_managed_agents(self.app)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        Ok(read(record)?)
    }
}

fn provider_target(record: &ManagedAgentRecord) -> Option<ProviderTarget> {
    match &record.backend {
        BackendKind::Provider { id, config } => Some(ProviderTarget {
            provider_id: id.clone(),
            provider_config: config.clone(),
            cached_binary: record.provider_binary_path.as_ref().map(PathBuf::from),
        }),
        BackendKind::Local => None,
    }
}

impl<R: tauri::Runtime> InvocationSource for DesktopAgents<'_, R> {
    fn target(&self, pubkey: &str) -> Result<Option<ProviderTarget>, SessionError> {
        self.with_record(pubkey, |record| Ok(provider_target(record)))
    }

    fn rebuild(&self, pubkey: &str) -> Result<Invocation, SessionError> {
        self.with_record(pubkey, |record| {
            let target = provider_target(record)
                .ok_or_else(|| format!("agent {pubkey} is not provider-backed"))?;
            Ok(Invocation {
                target,
                payload: build_deploy_payload(self.app, self.state, record)?,
            })
        })
    }

    /// Persist the result (backend_agent_id or last_error) under the store
    /// lock before the deploy's outcome is returned.
    fn record_deploy(
        &self,
        pubkey: &str,
        result: &Result<String, SessionError>,
        payload: &serde_json::Value,
    ) -> Result<(), SessionError> {
        let _store_guard = self
            .state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(self.app)?;
        let record = records
            .iter_mut()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        apply_deploy_result(record, result, payload);
        save_managed_agents(self.app, &records)?;
        Ok(())
    }
}

/// Deploy an agent to a provider backend through [`RemoteSessions::deploy`]:
/// the per-agent lock, the post-lock rebuild, the tenant check against the
/// rebuilt payload, and the session binding all happen there. The record is
/// updated and saved before returning.
///
/// Idempotency: calling deploy on an already-deployed agent sends the same payload
/// again. Providers are expected to handle this as an update-in-place or no-op.
/// Thread-sandbox providers end a session with the separate `stop` operation
/// (`end_remote_agent_session`); other providers have no undeploy operation, so
/// a successful redeploy delegates access-policy revocation semantics to the
/// provider implementation.
/// Returns Ok(()) on success, Err on failure, with the provider's refusal code
/// when it sent one.
///
/// `replay_floor_unix`: optional unix-seconds replay floor from a
/// publish-first mention send, injected as `BUZZ_ACP_REPLAY_FLOOR` for this
/// invocation only.
pub(crate) async fn deploy_to_provider<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    pubkey: &str,
    tenant: Tenant<'_>,
    session: Option<&SessionScope>,
    replay_floor_unix: Option<u64>,
) -> Result<(), ProviderFailure> {
    Ok(remote_sessions(app, state)
        .deploy(pubkey, tenant, session, replay_floor_unix)
        .await?)
}

fn policy_matches_payload(
    record: &ManagedAgentRecord,
    deployed_agent_json: &serde_json::Value,
) -> bool {
    deployed_agent_json
        .get("respond_to")
        .and_then(serde_json::Value::as_str)
        == Some(record.respond_to.as_str())
        && deployed_agent_json.get("respond_to_allowlist")
            == Some(&serde_json::json!(record.respond_to_allowlist))
}

fn apply_deploy_result(
    record: &mut ManagedAgentRecord,
    deploy_result: &Result<String, SessionError>,
    deployed_agent_json: &serde_json::Value,
) {
    match deploy_result {
        Ok(backend_agent_id) => {
            record.backend_agent_id = Some(backend_agent_id.clone());
            if policy_matches_payload(record, deployed_agent_json) {
                record.provider_policy_pending = false;
            }
            record.last_started_at = Some(now_iso());
            record.updated_at = now_iso();
            record.last_error = None;
        }
        Err(error) => {
            record.last_error = Some(error.message.clone());
            record.updated_at = now_iso();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ManagedAgentRecord {
        serde_json::from_value(serde_json::json!({
            "pubkey": "agent", "name": "Agent", "relay_url": "", "acp_command": "",
            "agent_command": "", "agent_args": [], "mcp_command": "",
            "turn_timeout_seconds": 0, "system_prompt": null, "created_at": "",
            "updated_at": "", "last_started_at": null, "last_stopped_at": null,
            "last_exit_code": null, "last_error": null,
            "provider_policy_pending": true
        }))
        .unwrap()
    }

    fn policy_payload(respond_to: &str) -> serde_json::Value {
        serde_json::json!({"respond_to": respond_to, "respond_to_allowlist": []})
    }

    #[test]
    fn successful_deploy_acknowledges_pending_policy() {
        let mut record = record();

        apply_deploy_result(
            &mut record,
            &Ok("provider-agent".into()),
            &policy_payload("owner-only"),
        );

        assert!(!record.provider_policy_pending);
        assert_eq!(record.backend_agent_id.as_deref(), Some("provider-agent"));
        assert_eq!(record.last_error, None);
    }

    #[test]
    fn successful_stale_deploy_preserves_newer_pending_policy() {
        let mut record = record();
        record.respond_to = crate::managed_agents::RespondTo::Anyone;

        apply_deploy_result(
            &mut record,
            &Ok("provider-agent".into()),
            &policy_payload("owner-only"),
        );

        assert!(record.provider_policy_pending);
    }

    #[test]
    fn failed_deploy_preserves_pending_policy() {
        let mut record = record();

        apply_deploy_result(
            &mut record,
            &Err(SessionError {
                message: "provider unavailable".into(),
                code: Some("session_ended".into()),
            }),
            &policy_payload("owner-only"),
        );

        assert_eq!(record.backend_agent_id, None);
        assert!(record.provider_policy_pending);
        assert_eq!(record.last_error.as_deref(), Some("provider unavailable"));
    }
}
