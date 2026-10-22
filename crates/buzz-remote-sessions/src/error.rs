/// A failed provider operation, or a session action that ended in one.
///
/// `code` is the provider's stable refusal code (`docs/remote-agents.md`
/// §Sandbox error codes). It is absent for transient failures, client-side
/// failures, and providers that predate codes. Branch on the code with
/// [`recovery_kind`](crate::recovery_kind); `message` is for display.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SessionError {
    pub message: String,
    pub code: Option<String>,
}

impl From<String> for SessionError {
    fn from(message: String) -> Self {
        Self {
            message,
            code: None,
        }
    }
}

impl From<&str> for SessionError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

impl From<SessionError> for String {
    fn from(error: SessionError) -> Self {
        error.message
    }
}
