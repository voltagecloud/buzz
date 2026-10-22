//! The desktop's adapter over `buzz-remote-sessions`: the Tauri command
//! error shape, the bundled-provider search directory, and the agent working
//! directory providers run in.

use std::path::{Path, PathBuf};

pub(crate) use buzz_remote_sessions::{
    redact_env_values_in, redact_secrets_with, validate_provider_config,
};
use buzz_remote_sessions::{Discovery, SessionError};

/// A failed provider operation, or a command that ended in one.
///
/// `code` is the provider's stable refusal code (`docs/remote-agents.md`
/// §Sandbox error codes). It is absent for transient failures, desktop-side
/// failures, and providers that predate codes. As a Tauri command error it
/// serializes as the plain message when there is no code, and as
/// `{"message": ..., "code": ...}` when there is one; the frontend's
/// `invokeTauri` accepts both shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub message: String,
    pub code: Option<String>,
}

impl From<SessionError> for ProviderFailure {
    fn from(SessionError { message, code }: SessionError) -> Self {
        Self { message, code }
    }
}

impl From<String> for ProviderFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            code: None,
        }
    }
}

impl From<&str> for ProviderFailure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

impl From<ProviderFailure> for String {
    fn from(failure: ProviderFailure) -> Self {
        failure.message
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl serde::Serialize for ProviderFailure {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let Some(code) = &self.code else {
            return serializer.serialize_str(&self.message);
        };
        let mut failure = serializer.serialize_struct("ProviderFailure", 2)?;
        failure.serialize_field("message", &self.message)?;
        failure.serialize_field("code", code)?;
        failure.end()
    }
}

/// Where the desktop finds `buzz-backend-*` providers: the executable's
/// directory first (`Contents/MacOS/` in a `.app` bundle, where Tauri
/// stages bundled providers), so they are found even when the process PATH
/// is minimal; then `PATH` and `~/.local/bin`. Read per operation.
pub(crate) fn provider_discovery() -> Discovery {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    Discovery::from_env(bundled)
}

/// Enumerate the discovered `buzz-backend-*` executables. Returns (id, path)
/// pairs. Does NOT execute any binaries.
pub fn discover_provider_candidates() -> Vec<(String, PathBuf)> {
    provider_discovery().candidates()
}

/// Resolve a provider ID to a discovered, executable binary path. See
/// [`buzz_remote_sessions::resolve_provider_binary`].
pub fn resolve_provider_binary(provider_id: &str) -> Result<PathBuf, String> {
    buzz_remote_sessions::resolve_provider_binary(provider_id, None, &provider_discovery())
        .map_err(String::from)
}

/// Invoke a provider binary in the agent working directory.
pub fn invoke_provider(
    binary: &Path,
    request: &serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, ProviderFailure> {
    Ok(buzz_remote_sessions::invoke_provider(
        binary,
        super::default_agent_workdir().as_deref(),
        request,
        timeout,
    )?)
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendProviderInfo {
    pub id: String,
    pub binary_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_serializes_as_text_without_a_code_and_an_object_with_one() {
        let uncoded = ProviderFailure::from(SessionError::from("provider unavailable"));
        assert_eq!(
            serde_json::to_value(&uncoded).unwrap(),
            "provider unavailable"
        );
        let coded = ProviderFailure::from(SessionError {
            message: "ended".into(),
            code: Some("session_ended".into()),
        });
        assert_eq!(
            serde_json::to_value(&coded).unwrap(),
            serde_json::json!({"message": "ended", "code": "session_ended"})
        );
    }
}
