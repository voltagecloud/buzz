//! Explicit lifecycle actions for a thread-sandbox agent's remote session:
//! end it, or recover it after it ended. Both run only on user action and
//! share the deploy path's per-agent lock, post-lock payload rebuild, tenant
//! scope assertion, and session binding.
//!
//! Errors carry the provider's refusal `code` when it sent one (see
//! [`ProviderFailure`]), so the frontend branches on the code, not the text.

use tauri::{AppHandle, State};

use crate::{
    app_state::AppState,
    managed_agents::{
        load_managed_agents, provider_ops, provider_stop, BackendKind, ManagedAgentSummary,
        ProviderFailure,
    },
};

use super::provider_deploy::{
    deploy_to_provider, lock_provider_operations, prepare_provider_invocation,
    resolve_record_provider_binary, InvocationScope, ProviderAction, ProviderInvocation,
    RemoteRecoveryMode, RemoteSessionScope,
};

/// The `info.ops` entry a provider advertises when it can end a thread
/// session (`stop`) and recover an ended one (`provider_config.sandbox.recovery`).
const RECOVER_OP: &str = "recover";

/// The tenant a session action was offered for. Both parts are required: an
/// action on a remote session is never unscoped.
fn session_tenant<'a>(
    expected_relay_url: &'a str,
    expected_signer_pubkey: &'a str,
    session: &'a RemoteSessionScope,
    action: ProviderAction,
) -> Result<InvocationScope<'a>, ProviderFailure> {
    if expected_relay_url.trim().is_empty() || expected_signer_pubkey.trim().is_empty() {
        return Err("remote session actions require the community and identity they were offered for; nothing was changed".into());
    }
    Ok(InvocationScope {
        expected_relay_url: Some(expected_relay_url),
        expected_signer_pubkey: Some(expected_signer_pubkey),
        session: Some(session),
        action,
    })
}

/// End the remote session for one channel thread with the provider's `stop`
/// operation. Returns the session state the provider reports: `ending`,
/// `ended`, or `absent`. The agent record is not changed: the provider owns
/// the session lifecycle, and recovery is a separate explicit action.
#[tauri::command]
pub async fn end_remote_agent_session(
    pubkey: String,
    session_scope: RemoteSessionScope,
    expected_relay_url: String,
    expected_signer_pubkey: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, ProviderFailure> {
    end_session(
        &app,
        &state,
        &pubkey,
        &session_scope,
        &expected_relay_url,
        &expected_signer_pubkey,
    )
    .await
}

async fn end_session<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    pubkey: &str,
    session_scope: &RemoteSessionScope,
    expected_relay_url: &str,
    expected_signer_pubkey: &str,
) -> Result<String, ProviderFailure> {
    let scope = session_tenant(
        expected_relay_url,
        expected_signer_pubkey,
        session_scope,
        ProviderAction::End,
    )?;
    let _guard = lock_provider_operations(state, pubkey).await?;
    let ProviderInvocation {
        binary,
        config,
        agent_json,
        ..
    } = prepare_provider_invocation(app, state, pubkey, &scope)?;
    tokio::task::spawn_blocking(move || provider_stop(&binary, &agent_json, &config))
        .await
        .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Recover the ended remote session for one channel thread, from its
/// checkpoint or fresh. The mode is sent for this deploy only and is never
/// persisted. Returns the updated agent, like `start_managed_agent`.
#[tauri::command]
pub async fn recover_remote_agent_session(
    pubkey: String,
    session_scope: RemoteSessionScope,
    mode: RemoteRecoveryMode,
    expected_relay_url: String,
    expected_signer_pubkey: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ManagedAgentSummary, ProviderFailure> {
    recover_session(
        &app,
        &state,
        &pubkey,
        &session_scope,
        mode,
        &expected_relay_url,
        &expected_signer_pubkey,
    )
    .await?;
    let _store_guard = state
        .managed_agents_store_lock
        .lock()
        .map_err(|e| e.to_string())?;
    let records = load_managed_agents(&app)?;
    let runtimes = state
        .managed_agent_processes
        .lock()
        .map_err(|e| e.to_string())?;
    let record = records
        .iter()
        .find(|r| r.pubkey == pubkey)
        .ok_or_else(|| format!("agent {pubkey} not found"))?;
    Ok(super::summarize_from_disk(&app, record, &runtimes)?)
}

/// The deploy half of [`recover_remote_agent_session`].
async fn recover_session<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    pubkey: &str,
    session_scope: &RemoteSessionScope,
    mode: RemoteRecoveryMode,
    expected_relay_url: &str,
    expected_signer_pubkey: &str,
) -> Result<(), ProviderFailure> {
    let scope = session_tenant(
        expected_relay_url,
        expected_signer_pubkey,
        session_scope,
        ProviderAction::Recover(mode),
    )?;
    deploy_to_provider(app, state, pubkey, &scope, None).await
}

/// Whether the agent's provider can end and recover thread sessions, which
/// it advertises as `recover` in `info.ops`. Runs only `info`. Local agents
/// and provider agents without thread workspaces return `false`.
#[tauri::command]
pub async fn remote_agent_session_supports_lifecycle(
    pubkey: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    supports_lifecycle(&app, &state, &pubkey).await
}

async fn supports_lifecycle<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    pubkey: &str,
) -> Result<bool, String> {
    let target = {
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|e| e.to_string())?;
        let records = load_managed_agents(app)?;
        let record = records
            .iter()
            .find(|r| r.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        match &record.backend {
            BackendKind::Provider { id, .. } if record.backend.is_thread_sandbox() => {
                Some((id.clone(), record.provider_binary_path.clone()))
            }
            _ => None,
        }
    };
    let Some((provider_id, cached_binary_path)) = target else {
        return Ok(false);
    };
    let binary = resolve_record_provider_binary(&provider_id, cached_binary_path.as_deref())?;
    let ops = tokio::task::spawn_blocking(move || provider_ops(&binary))
        .await
        .map_err(|e| format!("spawn_blocking failed: {e}"))??;
    Ok(ops.iter().any(|op| op == "stop") && ops.iter().any(|op| op == RECOVER_OP))
}

#[cfg(all(test, unix))]
#[path = "remote_session_tests.rs"]
mod tests;
