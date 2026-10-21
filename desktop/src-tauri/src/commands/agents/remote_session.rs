//! Explicit lifecycle actions for a thread-sandbox agent's remote session:
//! end it, or recover it after it ended. Both run only on user action, in
//! `buzz-remote-sessions`, which shares the deploy path's per-agent lock,
//! post-lock payload rebuild, tenant scope assertion, and session binding.
//!
//! Errors carry the provider's refusal `code` when it sent one (see
//! [`ProviderFailure`]), so the frontend branches on the code, not the text.

use buzz_remote_sessions::{RecoveryMode, SessionScope, StopState, Tenant};
use tauri::{AppHandle, State};

use crate::{
    app_state::AppState,
    managed_agents::{load_managed_agents, ManagedAgentSummary, ProviderFailure},
};

use super::provider_deploy::remote_sessions;

/// The tenant a session action was offered for. The client refuses the
/// action unless both parts are present.
fn session_tenant<'a>(expected_relay_url: &'a str, expected_signer_pubkey: &'a str) -> Tenant<'a> {
    Tenant {
        relay_url: Some(expected_relay_url),
        signer_pubkey: Some(expected_signer_pubkey),
    }
}

/// End the remote session for one channel thread with the provider's `stop`
/// operation. Returns the session state the provider reports: `ending`,
/// `ended`, or `absent`. The agent record is not changed: the provider owns
/// the session lifecycle, and recovery is a separate explicit action.
#[tauri::command]
pub async fn end_remote_agent_session(
    pubkey: String,
    session_scope: SessionScope,
    expected_relay_url: String,
    expected_signer_pubkey: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<StopState, ProviderFailure> {
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
    session_scope: &SessionScope,
    expected_relay_url: &str,
    expected_signer_pubkey: &str,
) -> Result<StopState, ProviderFailure> {
    Ok(remote_sessions(app, state)
        .end(
            pubkey,
            session_tenant(expected_relay_url, expected_signer_pubkey),
            session_scope,
        )
        .await?)
}

/// Recover the ended remote session for one channel thread, from its
/// checkpoint or fresh. The mode is sent for this deploy only and is never
/// persisted. Returns the updated agent, like `start_managed_agent`.
#[tauri::command]
pub async fn recover_remote_agent_session(
    pubkey: String,
    session_scope: SessionScope,
    mode: RecoveryMode,
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
    session_scope: &SessionScope,
    mode: RecoveryMode,
    expected_relay_url: &str,
    expected_signer_pubkey: &str,
) -> Result<(), ProviderFailure> {
    Ok(remote_sessions(app, state)
        .recover(
            pubkey,
            session_tenant(expected_relay_url, expected_signer_pubkey),
            session_scope,
            mode,
        )
        .await?)
}

/// Whether the agent's provider can end and recover thread sessions, which
/// it advertises as `stop` and `recover` in `info.ops`. Runs only `info`.
/// Local agents and provider agents without thread workspaces return `false`.
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
    Ok(remote_sessions(app, state)
        .supports_lifecycle(pubkey)
        .await?)
}

#[cfg(all(test, unix))]
#[path = "remote_session_tests.rs"]
mod tests;
