//! Remote workspace requests reach the host's backend, with custody, and a
//! host without one says so.

use buzz_host::api::*;
use buzz_host::credentials::FileCredentials;
use buzz_host::sandbox::{AgentCustody, SandboxBackend};
use buzz_host::{AgentHost, HostConfig};
use buzz_host_rpc::HostInfo;
use futures::future::BoxFuture;
use std::sync::Arc;

fn host(state: &std::path::Path, sandbox: Option<Arc<dyn SandboxBackend>>) -> Arc<AgentHost> {
    AgentHost::start(HostConfig {
        info: HostInfo {
            id: "devbox".into(),
            name: "Devbox".into(),
            platform: "linux".into(),
            version: "test".into(),
        },
        state_dir: state.join("state"),
        credentials: Arc::new(FileCredentials::new(state.join("keys")).unwrap()),
        legacy_dir: None,
        sandbox,
    })
    .unwrap()
}

async fn sandbox(host: &AgentHost, request: SandboxRequest) -> SandboxResponse {
    match host.request(Request::Sandbox(request)).await.unwrap() {
        Response::Sandbox(response) => response,
        other => panic!("expected a sandbox response, got {other:?}"),
    }
}

/// Answers whether custody has the agent.
struct Lookup;

impl SandboxBackend for Lookup {
    fn serve(
        &self,
        request: SandboxRequest,
        custody: Arc<dyn AgentCustody>,
    ) -> BoxFuture<'static, SandboxResponse> {
        Box::pin(async move {
            let SandboxRequest::Lifecycle { pubkey, .. } = request else {
                return SandboxResponse::Started;
            };
            match custody.launch_identity(&pubkey).await {
                Ok(_) => SandboxResponse::Lifecycle(true),
                Err(message) => SandboxResponse::Failed {
                    message,
                    code: Some("no_agent".into()),
                },
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_without_a_backend_answers_unsupported() {
    let state = tempfile::tempdir().unwrap();
    let host = host(state.path(), None);
    match sandbox(&host, SandboxRequest::Providers).await {
        SandboxResponse::Failed { code, .. } => {
            assert_eq!(code.as_deref(), Some(SANDBOX_UNSUPPORTED))
        }
        other => panic!("expected unsupported, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_reach_the_backend_with_custody() {
    let state = tempfile::tempdir().unwrap();
    let host = host(state.path(), Some(Arc::new(Lookup)));
    let lifecycle = SandboxRequest::Lifecycle {
        pubkey: "a".repeat(64),
        spec: LaunchSpec::default(),
    };
    match sandbox(&host, lifecycle).await {
        SandboxResponse::Failed { message, code } => {
            assert_eq!(code.as_deref(), Some("no_agent"));
            assert_eq!(message, "No such agent on this device");
        }
        other => panic!("expected the agent to be missing, got {other:?}"),
    }
}
