//! The process bridge Claude Code and Codex agents run through: declared processes
//! only, output in order to the connection that spawned them, and killed
//! with that connection.

use buzz_host::api::*;
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, EventSource, HostClient, HostClientOptions, HostHandler,
    HostInfo, Peer, Protocol, RpcError, ServerOptions, serve, transport,
};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

/// Collects each handle's output, as the GUI's process sinks do.
#[derive(Default)]
struct Outputs {
    sinks: Mutex<HashMap<u64, mpsc::UnboundedSender<ProcessOutput>>>,
}

impl Outputs {
    fn watch(&self, handle: u64) -> mpsc::UnboundedReceiver<ProcessOutput> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.sinks.lock().unwrap().insert(handle, sender);
        receiver
    }
}

struct Gui(Arc<Outputs>);

impl ClientHandler<HostProtocol> for Gui {
    fn handle(&self, ask: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        let answer = match ask {
            HostAsk::ProcessOutput { handle, output } => {
                let sinks = self.0.sinks.lock().unwrap();
                match sinks.get(&handle) {
                    Some(sink) => {
                        for item in output {
                            let _ = sink.send(item);
                        }
                        Ok(HostAnswer::ProcessReceived)
                    }
                    None => Err(RpcError::Failed("No such process".into())),
                }
            }
        };
        Box::pin(async move { answer })
    }
}

/// Buzz's copy of Claude, under this binary's own data folder, as a
/// `claude` that runs `$FAKE_CLAUDE` as a shell script, so each test picks
/// its behaviour through the spawn environment.
fn fake_bin() -> &'static Path {
    static DATA: OnceLock<tempfile::TempDir> = OnceLock::new();
    DATA.get_or_init(|| {
        let data = tempfile::tempdir().unwrap();
        let copies = data.path().join("buzz/claude-code");
        std::fs::create_dir_all(copies.join("2.1.130")).unwrap();
        std::fs::write(copies.join("current"), "2.1.130").unwrap();
        let claude = copies.join("2.1.130/claude");
        std::fs::write(&claude, "#!/bin/sh\neval \"$FAKE_CLAUDE\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: set once, before any host in this binary spawns.
        unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };
        data
    })
    .path()
}

struct Setup {
    _state: tempfile::TempDir,
    connector: Arc<dyn transport::Connector>,
}

fn start_host() -> Setup {
    fake_bin();
    let state = tempfile::tempdir().unwrap();
    let host = AgentHost::start(HostConfig {
        info: HostInfo {
            id: "laptop".into(),
            name: "This laptop".into(),
            platform: "linux".into(),
            version: "test".into(),
        },
        state_dir: state.path().join("state"),
        credentials: Arc::new(FileCredentials::new(state.path().join("keys")).unwrap()),
        legacy_dir: None,
        sandbox: None,
    })
    .unwrap();
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host)),
        ServerOptions::default(),
    ));
    Setup {
        _state: state,
        connector: Arc::new(connector),
    }
}

async fn connect(
    connector: &Arc<dyn transport::Connector>,
) -> (HostClient<HostProtocol>, Arc<Outputs>) {
    let outputs = Arc::new(Outputs::default());
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        connector.clone(),
        Arc::new(Gui(outputs.clone())),
        HostClientOptions::default(),
    );
    client
        .state()
        .wait_for(|state| matches!(state, ConnectionState::Connected { .. }))
        .await
        .unwrap();
    (client, outputs)
}

fn spawn(handle: u64, process: &str, args: &[&str], script: Option<&str>) -> Request {
    let mut env = BTreeMap::new();
    if let Some(script) = script {
        env.insert("FAKE_CLAUDE".to_string(), Some(script.to_string()));
    }
    Request::ProcessSpawn {
        handle,
        owner: "claude-code".into(),
        process: process.into(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        cwd: None,
        env,
    }
}

/// Everything a process printed to stdout and stderr, and its exit code.
async fn finish(
    receiver: &mut mpsc::UnboundedReceiver<ProcessOutput>,
) -> (String, String, Option<i32>) {
    let (mut stdout, mut stderr) = (String::new(), String::new());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match receiver.recv().await.expect("output ended without an exit") {
                ProcessOutput::Stdout(text) => stdout += &text,
                ProcessOutput::Stderr(text) => stderr += &text,
                ProcessOutput::Exit(code) => return (stdout, stderr, code),
            }
        }
    })
    .await
    .expect("the process did not exit")
}

#[tokio::test(flavor = "multi_thread")]
async fn declared_processes_run_and_report_in_order() {
    let setup = start_host();
    let (client, outputs) = connect(&setup.connector).await;

    // `read` is `base64 -i`, which passes a file through intact.
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), [0u8, 159, 146, 150, b'x']).unwrap();
    let mut read = outputs.watch(1);
    client
        .request(spawn(1, "read", &[file.path().to_str().unwrap()], None))
        .await
        .unwrap();
    let (stdout, _, code) = finish(&mut read).await;
    assert_eq!((stdout.trim(), code), ("AJ+Slng=", Some(0)));

    // Many lines arrive in the order they were written.
    let mut lines = outputs.watch(2);
    client
        .request(spawn(
            2,
            "claude",
            &[],
            Some("i=0; while [ $i -lt 3000 ]; do echo line$i; i=$((i+1)); done; echo oops >&2; exit 3"),
        ))
        .await
        .unwrap();
    let (stdout, stderr, code) = finish(&mut lines).await;
    let expected: String = (0..3000).map(|i| format!("line{i}\n")).collect();
    assert_eq!(stdout, expected);
    assert_eq!((stderr.as_str(), code), ("oops\n", Some(3)));

    // Stdin is written and ended; null removes a variable; a missing
    // working directory is created.
    let cwd = tempfile::tempdir().unwrap();
    let nested = cwd.path().join("made/here");
    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_CLAUDE".to_string(),
        Some("cat; pwd; echo \"[$HOME_GONE]\"".to_string()),
    );
    env.insert("HOME_GONE".to_string(), None);
    let mut echo = outputs.watch(3);
    client
        .request(Request::ProcessSpawn {
            handle: 3,
            owner: "claude-code".into(),
            process: "claude".into(),
            args: vec![],
            cwd: Some(nested.to_string_lossy().into_owned()),
            env,
        })
        .await
        .unwrap();
    client
        .request(Request::ProcessWrite {
            handle: 3,
            data: "hello\n".into(),
            close: false,
        })
        .await
        .unwrap();
    client
        .request(Request::ProcessWrite {
            handle: 3,
            data: "bye\n".into(),
            close: true,
        })
        .await
        .unwrap();
    let (stdout, _, code) = finish(&mut echo).await;
    assert_eq!(
        stdout,
        format!("hello\nbye\n{}\n[]\n", nested.display()),
        "{stdout}"
    );
    assert_eq!(code, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_declares_its_workspace_reader_and_server() {
    let setup = start_host();
    let (client, outputs) = connect(&setup.connector).await;
    let codex = |handle, process: &str, args: &[&str], cwd: Option<String>| Request::ProcessSpawn {
        handle,
        owner: "codex".into(),
        process: process.into(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        cwd,
        env: BTreeMap::new(),
    };
    // `workspace` prints the directory the host made for it.
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("made/.buzz");
    let mut pwd = outputs.watch(1);
    client
        .request(codex(
            1,
            "workspace",
            &[],
            Some(workspace.to_string_lossy().into_owned()),
        ))
        .await
        .unwrap();
    let (stdout, _, code) = finish(&mut pwd).await;
    assert_eq!(
        (std::path::PathBuf::from(stdout.trim()), code),
        (workspace.canonicalize().unwrap(), Some(0))
    );
    let file = workspace.join("answer.txt");
    std::fs::write(&file, "hi").unwrap();
    let mut read = outputs.watch(2);
    client
        .request(codex(
            2,
            "read",
            &["answer.txt"],
            Some(workspace.to_string_lossy().into_owned()),
        ))
        .await
        .unwrap();
    let (stdout, _, code) = finish(&mut read).await;
    assert_eq!((stdout.trim(), code), ("aGk=", Some(0)));
    // Codex's own processes are not Claude Code's.
    assert_eq!(
        client
            .request(codex(3, "claude", &[], None))
            .await
            .unwrap_err()
            .to_string(),
        "Process is not declared"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn undeclared_processes_and_bad_input_are_refused() {
    let setup = start_host();
    let (client, _outputs) = connect(&setup.connector).await;
    let refused = |request| async { client.request(request).await.unwrap_err().to_string() };
    let mut sh = spawn(1, "sh", &["-c", "true"], None);
    assert_eq!(refused(sh.clone()).await, "Process is not declared");
    if let Request::ProcessSpawn { owner, process, .. } = &mut sh {
        *owner = "other-type".into();
        *process = "claude".into();
    }
    assert_eq!(refused(sh).await, "Process is not declared");
    let mut relative = spawn(2, "claude", &[], Some("true"));
    if let Request::ProcessSpawn { cwd, .. } = &mut relative {
        *cwd = Some("relative/dir".into());
    }
    assert_eq!(
        refused(relative).await,
        "The process directory must be absolute"
    );
    assert_eq!(
        refused(Request::ProcessKill { handle: 99 }).await,
        "No such process"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_stops_a_process_and_only_its_connection_may() {
    let setup = start_host();
    let (client, outputs) = connect(&setup.connector).await;
    let (other, _) = connect(&setup.connector).await;
    let mut sleeper = outputs.watch(7);
    client
        .request(spawn(7, "claude", &[], Some("echo up; sleep 30")))
        .await
        .unwrap();
    assert_eq!(
        sleeper.recv().await,
        Some(ProcessOutput::Stdout("up\n".into()))
    );
    // Another GUI cannot reach it by handle.
    assert_eq!(
        other
            .request(Request::ProcessKill { handle: 7 })
            .await
            .unwrap_err()
            .to_string(),
        "No such process"
    );
    client
        .request(Request::ProcessKill { handle: 7 })
        .await
        .unwrap();
    let (_, _, code) = finish(&mut sleeper).await;
    assert_ne!(code, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn processes_die_with_their_connection() {
    let setup = start_host();
    let (client, outputs) = connect(&setup.connector).await;
    let mut sleeper = outputs.watch(1);
    client
        .request(spawn(1, "claude", &[], Some("echo $$; exec sleep 30")))
        .await
        .unwrap();
    let Some(ProcessOutput::Stdout(pid)) = sleeper.recv().await else {
        panic!("no pid");
    };
    let pid: i32 = pid.trim().parse().unwrap();
    // SAFETY: probes the test's own child.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    drop(client);
    tokio::time::timeout(Duration::from_secs(10), async {
        // SAFETY: as above.
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the process outlived its connection");
}

/// A host from before the process bridge: it knows only the first requests.
struct OldHost(tokio::sync::broadcast::Sender<(u64, HostEvent)>);

#[derive(Serialize, Deserialize)]
enum OldRequest {
    Agents,
}

struct Old;

impl Protocol for Old {
    type ClientRequest = OldRequest;
    type ClientResponse = Response;
    type HostRequest = HostAsk;
    type HostResponse = HostAnswer;
    type Event = HostEvent;
}

impl HostHandler<Old> for OldHost {
    fn info(&self) -> HostInfo {
        HostInfo {
            id: "old".into(),
            name: "Old".into(),
            platform: "linux".into(),
            version: "0".into(),
        }
    }
    fn events(&self) -> EventSource<Old> {
        self.0.subscribe()
    }
    fn handle(
        &self,
        _: Peer<Old>,
        request: OldRequest,
    ) -> BoxFuture<'static, Result<Response, RpcError>> {
        let OldRequest::Agents = request;
        Box::pin(async { Ok(Response::Done) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn old_hosts_refuse_process_requests_and_stay_connected() {
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Old, _>(
        listener,
        Arc::new(OldHost(tokio::sync::broadcast::channel(1).0)),
        ServerOptions::default(),
    ));
    let connector: Arc<dyn transport::Connector> = Arc::new(connector);
    let (client, _) = connect(&connector).await;
    let error = client
        .request(spawn(1, "claude", &[], None))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("update it"), "{error}");
    assert!(matches!(
        *client.state().borrow(),
        buzz_host_rpc::ConnectionState::Connected { .. }
    ));
}
