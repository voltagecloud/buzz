//! Remote workspaces: agents whose sessions run on a provider the host
//! starts, such as a Kubernetes sandbox per thread. The host adds what only
//! it holds, the agent's key and owner attestation, to the launch the GUI
//! describes; the key never comes back. A provider's refusal comes back as
//! [`SandboxResponse::Failed`] with its code, which an error could not
//! carry. JSON payloads travel as JSON text.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The code a host without remote workspaces fails with.
pub const SANDBOX_UNSUPPORTED: &str = "unsupported";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SandboxRequest {
    /// The providers this host can start. Answers
    /// [`SandboxResponse::Providers`].
    Providers,
    /// Starts or updates the agent's session for a thread. Answers
    /// [`SandboxResponse::Started`].
    Start {
        session: SandboxSession,
        /// Unix seconds the harness replays from, so it sees the message
        /// that started it.
        #[serde(default)]
        replay_floor: Option<u64>,
    },
    /// Ends the thread's session. Answers [`SandboxResponse::Ended`].
    End { session: SandboxSession },
    /// Brings back the thread's ended session. Answers
    /// [`SandboxResponse::Started`].
    Recover {
        session: SandboxSession,
        mode: RecoveryMode,
    },
    /// Whether the agent's provider can end and recover sessions. Answers
    /// [`SandboxResponse::Lifecycle`].
    Lifecycle { pubkey: String, spec: LaunchSpec },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SandboxResponse {
    Providers(Vec<SandboxProvider>),
    Started,
    Ended {
        state: EndState,
    },
    Lifecycle(bool),
    /// `code` is the provider's refusal code, such as `session_ended`, or
    /// [`SANDBOX_UNSUPPORTED`]; `None` for a failure without one. Codes a
    /// GUI does not know pass through.
    Failed {
        message: String,
        #[serde(default)]
        code: Option<String>,
    },
}

impl SandboxResponse {
    /// What a host without a sandbox backend answers.
    pub fn unsupported() -> Self {
        Self::Failed {
            message: "This host does not run remote workspaces.".into(),
            code: Some(SANDBOX_UNSUPPORTED.into()),
        }
    }
}

/// One agent's session in one thread, as the GUI asks for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSession {
    pub pubkey: String,
    pub tenant: SandboxTenant,
    pub scope: SandboxScope,
    pub spec: LaunchSpec,
}

/// The community and signed-in person the GUI asks for, which the host
/// checks against the agent it holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxTenant {
    /// The community's `wss://` origin.
    pub relay: String,
    /// The viewer's pubkey, as hex.
    pub signer: String,
}

/// The thread a session belongs to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxScope {
    /// The channel's UUID.
    pub channel_id: String,
    /// The thread's root event id, as lowercase hex.
    pub thread_root: String,
}

/// What the agent's type says to launch. Never the agent's key, community
/// or attestation: the host adds those, and refuses them here.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchSpec {
    /// The provider's id, such as `kubernetes`.
    pub provider: String,
    /// The provider's config, as JSON text.
    pub provider_config_json: String,
    /// The user's environment for the agent.
    pub env: BTreeMap<String, String>,
    pub launch: Launch,
    /// The agent's display name.
    pub name: String,
}

/// The harness to run and its environment.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Launch {
    /// A command name, found on the provider's PATH.
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Defaults the user's environment overrides.
    pub policy_env: BTreeMap<String, String>,
}

/// Environment values may be secrets: only their names are shown.
impl std::fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("provider", &self.provider)
            .field("provider_config_json", &self.provider_config_json)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("launch", &self.launch)
            .field("name", &self.name)
            .finish()
    }
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launch")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("policy_env", &self.policy_env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// How [`SandboxRequest::Recover`] brings a session back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryMode {
    /// From the ended session's checkpoint.
    Checkpoint,
    /// With an empty workspace.
    Fresh,
}

/// Where [`SandboxRequest::End`] left the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndState {
    /// Still stopping; ask again.
    Ending,
    Ended,
    /// The thread had none.
    Absent,
}

/// A provider the host found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxProvider {
    pub id: String,
    pub path: String,
    pub version: String,
    /// The operations it supports, such as `stop` and `recover`.
    pub ops: Vec<String>,
    /// Its config's JSON Schema, as JSON text.
    pub config_schema_json: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Request, Response};

    fn wire<T: Serialize>(value: &T) -> Vec<u8> {
        rmp_serde::to_vec_named(value).unwrap()
    }

    /// The requests as hosts from before remote workspaces have them.
    mod old {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Serialize, Deserialize)]
        pub enum Request {
            TerminalOwner,
        }
    }

    fn session() -> SandboxSession {
        SandboxSession {
            pubkey: "a".repeat(64),
            tenant: SandboxTenant {
                relay: "wss://relay.example".into(),
                signer: "b".repeat(64),
            },
            scope: SandboxScope {
                channel_id: "6f1c3c7e-8f0e-4b8e-9d55-1a3c1f5b2e10".into(),
                thread_root: "c".repeat(64),
            },
            spec: LaunchSpec {
                provider: "kubernetes".into(),
                provider_config_json: r#"{"sandbox":true}"#.into(),
                env: [("API_TOKEN".into(), "hunter2".into())].into(),
                launch: Launch {
                    command: "buzz-acp".into(),
                    policy_env: [("BUZZ_ACP_TIMEOUT".into(), "600".into())].into(),
                    ..Default::default()
                },
                name: "Ada".into(),
            },
        }
    }

    #[test]
    fn requests_and_answers_round_trip() {
        let start = Request::Sandbox(SandboxRequest::Start {
            session: session(),
            replay_floor: Some(1_700_000_000),
        });
        match rmp_serde::from_slice(&wire(&start)).unwrap() {
            Request::Sandbox(SandboxRequest::Start {
                session: read,
                replay_floor,
            }) => {
                assert_eq!(read, session());
                assert_eq!(replay_floor, Some(1_700_000_000));
            }
            other => panic!("expected Start, got {other:?}"),
        }
        let recover = SandboxRequest::Recover {
            session: session(),
            mode: RecoveryMode::Checkpoint,
        };
        assert!(matches!(
            rmp_serde::from_slice(&wire(&recover)).unwrap(),
            SandboxRequest::Recover {
                mode: RecoveryMode::Checkpoint,
                ..
            }
        ));
        let failed = Response::Sandbox(SandboxResponse::Failed {
            message: "The session ended".into(),
            code: Some("a_code_from_later".into()),
        });
        assert!(matches!(
            rmp_serde::from_slice(&wire(&failed)).unwrap(),
            Response::Sandbox(SandboxResponse::Failed { code: Some(code), .. })
                if code == "a_code_from_later"
        ));
        let ended = SandboxResponse::Ended {
            state: EndState::Ending,
        };
        assert!(matches!(
            rmp_serde::from_slice(&wire(&ended)).unwrap(),
            SandboxResponse::Ended {
                state: EndState::Ending
            }
        ));
    }

    #[test]
    fn older_hosts_refuse_sandbox_requests() {
        let start = Request::Sandbox(SandboxRequest::End { session: session() });
        // An older host cannot decode it, so it answers that it does not
        // support the request.
        assert!(rmp_serde::from_slice::<old::Request>(&wire(&start)).is_err());
        assert!(matches!(
            rmp_serde::from_slice(&wire(&old::Request::TerminalOwner)).unwrap(),
            Request::TerminalOwner
        ));
    }

    #[test]
    fn debug_hides_environment_values() {
        let shown = format!("{:?}", SandboxRequest::End { session: session() });
        assert!(shown.contains("API_TOKEN") && shown.contains("BUZZ_ACP_TIMEOUT"));
        assert!(
            !shown.contains("hunter2") && !shown.contains("600"),
            "{shown}"
        );
    }
}
