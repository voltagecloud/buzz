//! Session inputs for one provider invocation and the pure checks that bind
//! them to the payload actually invoked. None of them are persisted.

use crate::scope::{assert_expected_relay_scope, assert_expected_signer, relay_http_base_url};
use crate::SessionError;

/// The harness environment variable carrying a publish-first replay floor
/// (unix seconds). The local spawn sets the same variable.
pub const REPLAY_FLOOR_ENV_VAR: &str = "BUZZ_ACP_REPLAY_FLOOR";

/// Published channel and canonical thread that own a remote workspace.
///
/// The fields are plain wire values. [`SessionScope::new`] validates them,
/// and every invocation re-checks a deserialized scope before using it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionScope {
    pub channel_id: uuid::Uuid,
    pub thread_root: String,
}

impl SessionScope {
    /// A scope for `thread_root`, the thread's lowercase 64-hex root event id.
    pub fn new(channel_id: uuid::Uuid, thread_root: String) -> Result<Self, SessionError> {
        let scope = Self {
            channel_id,
            thread_root,
        };
        scope.check()?;
        Ok(scope)
    }

    fn check(&self) -> Result<(), String> {
        if self.thread_root.len() != 64
            || !self
                .thread_root
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("Remote workspace requires a canonical thread root".into());
        }
        Ok(())
    }
}

/// How an explicit user action recovers an ended thread session. Sent as
/// `provider_config.sandbox.recovery` for one invocation only; never stored
/// on the agent record, so no later deploy can resurrect a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryMode {
    /// Restore the ended session's verified checkpoint.
    Checkpoint,
    /// Start a new session with an empty workspace.
    Fresh,
}

/// The tenant a caller captured: the community relay and signer identity it
/// validated before its first await.
///
/// Each part is asserted against the payload REBUILT after the per-agent
/// lock, the exact value invoked. `None` (or blank) preserves the unscoped
/// behavior for deploy callers without a tenant boundary; session actions
/// require both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tenant<'a> {
    pub relay_url: Option<&'a str>,
    pub signer_pubkey: Option<&'a str>,
}

impl Tenant<'_> {
    /// Refuse an action on a remote session that does not name the tenant
    /// it was offered for. A session action is never unscoped.
    pub(crate) fn require_both(self) -> Result<Self, SessionError> {
        if !has_expectation(self.relay_url) || !has_expectation(self.signer_pubkey) {
            return Err("remote session actions require the community and identity they were offered for; nothing was changed".into());
        }
        Ok(self)
    }
}

fn has_expectation(expected: Option<&str>) -> bool {
    expected.is_some_and(|value| !value.trim().is_empty())
}

/// What one provider invocation does for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Start or update the agent (an ordinary deploy).
    Deploy,
    /// End the thread's session with the provider's `stop`.
    End,
    /// Recover the thread's ended session: a deploy carrying the mode.
    Recover(RecoveryMode),
}

impl Action {
    pub(crate) fn recovery(self) -> Option<RecoveryMode> {
        match self {
            Self::Recover(mode) => Some(mode),
            Self::Deploy | Self::End => None,
        }
    }
}

/// The providers whose sessions can belong to threads: Kubernetes, and the
/// relay provider that asks a Sandbox manager to run them.
pub const THREAD_PROVIDERS: &[&str] = &["kubernetes", "relay"];

/// Whether launches require a published channel thread instead of app
/// lifecycle events: a [`THREAD_PROVIDERS`] provider with `sandbox: true`.
pub fn is_thread_sandbox(provider_id: &str, config: &serde_json::Value) -> bool {
    THREAD_PROVIDERS.contains(&provider_id)
        && config.get("sandbox") == Some(&serde_json::Value::Bool(true))
}

/// Replace a thread Sandbox's `sandbox: true` request with the published
/// thread and the rebuilt payload's verified owner. Callers decide eligibility
/// with [`is_thread_sandbox`] on the rebuilt record.
pub(crate) fn bind_sandbox_scope(
    config: &mut serde_json::Value,
    scope: Option<&SessionScope>,
    agent_pubkey: &str,
    payload: &serde_json::Value,
    recovery: Option<RecoveryMode>,
) -> Result<(), String> {
    let scope = scope.ok_or("Mention this agent in a channel thread to start its workspace")?;
    scope.check()?;
    let owner = payload
        .pointer("/launch/owner_pubkey")
        .and_then(|v| v.as_str())
        .ok_or("Remote workspace requires a verified owner")?;
    config["sandbox"] = serde_json::json!({
        "channel_id": scope.channel_id.to_string(), "thread_root": scope.thread_root,
    });
    if let Some(mode) = recovery {
        config["sandbox"]["recovery"] = serde_json::json!({ "mode": mode });
    }
    config["identity_policy"] = serde_json::json!({
        "agent_pubkey": agent_pubkey, "owner_pubkey": owner,
    });
    Ok(())
}

/// Assert a caller-captured tenant against the payload that will actually be
/// invoked. The relay lives at the payload's top-level `relay_url`; the
/// deploying identity lives at `launch.owner_pubkey` — both were re-resolved
/// from live host state by the rebuild, so this is the check tied to the use.
/// When the caller carries an expectation a missing payload field fails
/// closed: an unverifiable payload must never deploy on behalf of a scoped
/// callback.
pub(crate) fn assert_payload_scope(
    payload: &serde_json::Value,
    tenant: Tenant<'_>,
    action: Action,
) -> Result<(), String> {
    // The shared checks word a mismatch for a message send; say what this
    // action did instead, keeping the "active ... changed" prefix.
    let reword = |error: String, what: &str| match action {
        Action::Deploy => error,
        Action::End => {
            format!("active {what} changed before the session was ended; nothing was changed")
        }
        Action::Recover(_) => {
            format!("active {what} changed before the session was recovered; nothing was changed")
        }
    };
    match payload.get("relay_url").and_then(|v| v.as_str()) {
        Some(embedded_relay) => {
            assert_expected_relay_scope(tenant.relay_url, &relay_http_base_url(embedded_relay))
                .map_err(|error| reword(error, "community"))?
        }
        None if has_expectation(tenant.relay_url) => {
            return Err("deploy payload carries no relay; not deployed".to_string());
        }
        None => {}
    }
    match payload
        .get("launch")
        .and_then(|launch| launch.get("owner_pubkey"))
        .and_then(|v| v.as_str())
    {
        Some(owner) => assert_expected_signer(tenant.signer_pubkey, owner)
            .map_err(|error| reword(error, "identity"))?,
        None if has_expectation(tenant.signer_pubkey) => {
            return Err("deploy payload carries no owner identity; not deployed".to_string());
        }
        None => {}
    }
    Ok(())
}

/// Inject a caller-supplied replay floor into the deploy payload so the
/// remote harness consumes it exactly like a local spawn: as the
/// [`REPLAY_FLOOR_ENV_VAR`] environment variable. The floor rides
/// `launch.policy_env` (tier 1); any same-named key in `launch.env` (tier 2)
/// is stripped because that tier later-wins and a persisted user value must
/// not shadow this send's floor — the remote mirror of the local spawn's
/// post-env write. With no caller floor the payload is left untouched — a
/// user-supplied `launch.env` value passes through, and plain redeploys never
/// carry a stale floor.
pub(crate) fn apply_replay_floor(payload: &mut serde_json::Value, replay_floor_unix: Option<u64>) {
    let Some(floor) = replay_floor_unix else {
        return;
    };
    let Some(launch) = payload
        .get_mut("launch")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if let Some(env) = launch
        .get_mut("env")
        .and_then(serde_json::Value::as_object_mut)
    {
        let shadowed: Vec<String> = env
            .keys()
            .filter(|key| key.eq_ignore_ascii_case(REPLAY_FLOOR_ENV_VAR))
            .cloned()
            .collect();
        for key in shadowed {
            env.remove(&key);
        }
    }
    match launch
        .get_mut("policy_env")
        .and_then(serde_json::Value::as_object_mut)
    {
        Some(policy_env) => {
            policy_env.insert(
                REPLAY_FLOOR_ENV_VAR.to_string(),
                serde_json::Value::String(floor.to_string()),
            );
        }
        None => {
            launch.insert(
                "policy_env".to_string(),
                serde_json::json!({ (REPLAY_FLOOR_ENV_VAR): floor.to_string() }),
            );
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
