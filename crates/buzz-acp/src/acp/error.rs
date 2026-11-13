/// Errors that can occur in the ACP client.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Agent process exited unexpectedly")]
    AgentExited,

    #[error("Idle timeout — no agent activity for {0:?}")]
    IdleTimeout(std::time::Duration),

    #[error("Hard turn timeout exceeded (silence {silence:?})")]
    HardTimeout { silence: std::time::Duration },

    #[error("Agent did not stop within {0:?} after cancellation")]
    CancelDrainTimeout(std::time::Duration),

    #[error("Request timeout — agent did not respond within {0:?}")]
    Timeout(std::time::Duration),

    #[error("Write timeout — agent stopped reading stdin (blocked for {0:?})")]
    WriteTimeout(std::time::Duration),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Shared task permission: {0}")]
    TaskPermission(String),
    /// Retryable, fixed-reason refusal before durable handoff capacity is reserved.
    #[error("shared_task_retry: {0}")]
    HandoffAdmissionRetry(String),

    #[error("Agent reported error (code {code}): {message}")]
    AgentError { code: i64, message: String },

    /// The adapter cannot run the permission mode this harness requires.
    /// The pipe is intact, and neither a retry nor a respawn can repair it.
    #[error("Required permission mode unavailable: {0}")]
    PermissionModeUnavailable(String),
}
