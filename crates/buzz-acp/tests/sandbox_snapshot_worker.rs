#![cfg(target_os = "linux")]

//! Exercise the real contained worker; fixture helpers inject IPC and process failures.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio_util::codec::{FramedRead, LinesCodec};

const BOUND: Duration = Duration::from_secs(12);
const PRIVATE: &str = "fixture-private-value-do-not-return";
const PRELUDE: &str = r#"#!/usr/bin/env python3
import argparse,json,os,pathlib,signal,sys,time
p=argparse.ArgumentParser()
p.add_argument('--workspace');p.add_argument('--output');p.add_argument('--work-bytes',type=int)
a=p.parse_args();root=pathlib.Path(a.workspace);out=pathlib.Path(a.output)
def artifact(data=b'scanned fixture bytes',mode=0o600):
    fd=os.open(out,os.O_WRONLY|os.O_CREAT|os.O_EXCL,mode)
    with os.fdopen(fd,'wb') as f:f.write(data)
    return len(data)
def receipt(size,**changes):
    value=dict(version=1,scanned=True,repositories=1,bytes=size);value.update(changes)
    return json.dumps(value)
def identity(pid):
    text=pathlib.Path('/proc',str(pid),'stat').read_text()
    return dict(pid=pid,started=text.rsplit(')',1)[1].split()[19])
"#;
const TREE: &str = r#"
first=os.fork()
if first==0:
    os.setsid()
    second=os.fork()
    if second:os._exit(0)
    null=os.open('/dev/null',os.O_RDWR);os.dup2(null,1);os.dup2(null,2)
    marker=root/'grandchild.json';temporary=root/'grandchild.tmp'
    temporary.write_text(json.dumps(identity(os.getpid())));temporary.replace(marker)
    time.sleep(60);os._exit(0)
os.waitpid(first,0)
until=time.monotonic()+5
while not (root/'grandchild.json').exists():
    assert time.monotonic()<until
    time.sleep(.01)
grandchild=json.loads((root/'grandchild.json').read_text())
(root/'tree-pids.json').write_text(json.dumps([identity(os.getpid()),grandchild]))
"#;

fn supervisor() {
    static SETUP: Once = Once::new();
    SETUP.call_once(|| {
        // Failed containment mutations are adopted here and reaped by exact PID.
        nix::sys::prctl::set_child_subreaper(true).unwrap();
    });
}

struct Fixture {
    root: tempfile::TempDir,
    workspace: PathBuf,
    output: PathBuf,
    helper: PathBuf,
}
impl Fixture {
    fn new(body: &str) -> Self {
        supervisor();
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let helper = root.path().join("producer.py");
        std::fs::write(&helper, format!("{PRELUDE}\n{body}\n")).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = root.path().join("snapshot.tar");
        Self {
            root,
            workspace,
            output,
            helper,
        }
    }
}

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: FramedRead<ChildStdout, LinesCodec>,
    errors: Option<ChildStderr>,
}
impl Worker {
    fn start(fixture: &Fixture, seconds: u64) -> Self {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + seconds;
        let mut child = Command::new(env!("CARGO_BIN_EXE_buzz-acp"))
            .arg("sandbox-snapshot-worker")
            .arg(&fixture.workspace)
            .arg(&fixture.output)
            .arg(expiry.to_string())
            .arg("1048576")
            .arg(&fixture.helper)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &fixture.workspace)
            .env("BUZZ_PRIVATE_KEY", PRIVATE)
            .env("BUZZ_AUTH_TAG", PRIVATE)
            .env("OPENAI_API_KEY", PRIVATE)
            .env("ANTHROPIC_API_KEY", PRIVATE)
            .env("AWS_SECRET_ACCESS_KEY", PRIVATE)
            .env("SNAPSHOT_FIXTURE_AMBIENT_LEAK", PRIVATE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: FramedRead::new(
                child.stdout.take().unwrap(),
                LinesCodec::new_with_max_length(8192),
            ),
            errors: child.stderr.take(),
            child,
        }
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(BOUND, self.output.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(line.len() <= 4096);
        assert!(!line.contains(PRIVATE));
        assert!(!line.contains("DO_NOT_RETURN"));
        serde_json::from_str(&line).unwrap()
    }

    async fn started(&mut self) {
        assert_eq!(self.next().await, json!({"type":"started"}));
    }

    async fn finish(&mut self, code: i32) -> Value {
        let ended = self.next().await;
        assert_eq!(ended["type"], "ended");
        let status = tokio::time::timeout(BOUND, self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(code));
        assert!(tokio::time::timeout(BOUND, self.output.next())
            .await
            .unwrap()
            .is_none());
        let mut errors = Vec::new();
        if let Some(stream) = self.errors.take() {
            tokio::time::timeout(BOUND, stream.take(8193).read_to_end(&mut errors))
                .await
                .unwrap()
                .unwrap();
        }
        assert!(errors.len() <= 8192);
        let errors = String::from_utf8_lossy(&errors);
        assert!(!errors.contains(PRIVATE) && !errors.contains("DO_NOT_RETURN"));
        ended
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.input.take();
        if let Some(pid) = self.child.id() {
            let pid = nix::unistd::Pid::from_raw(pid as i32);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
            let deadline = std::time::Instant::now() + BOUND;
            while std::time::Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.start_kill();
        }
    }
}

#[derive(Clone, Deserialize)]
struct Identity {
    pid: i32,
    started: String,
}
fn started(pid: i32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(
        text.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)?
            .to_owned(),
    )
}
struct OwnedPid {
    identity: Identity,
    fd: std::os::fd::OwnedFd,
}
impl OwnedPid {
    fn capture(identity: &Identity) -> Option<Self> {
        if started(identity.pid).as_deref() != Some(identity.started.as_str()) {
            return None;
        }
        let fd = rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(identity.pid)?,
            rustix::process::PidfdFlags::empty(),
        )
        .ok()?;
        if started(identity.pid).as_deref() != Some(identity.started.as_str()) {
            return None;
        }
        Some(Self {
            identity: identity.clone(),
            fd,
        })
    }
}
impl Drop for OwnedPid {
    fn drop(&mut self) {
        let _ = rustix::process::pidfd_send_signal(&self.fd, rustix::process::Signal::KILL);
        if started(self.identity.pid).as_deref() == Some(self.identity.started.as_str()) {
            let pid = nix::unistd::Pid::from_raw(self.identity.pid);
            for _ in 0..100 {
                match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                    Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    _ => break,
                }
            }
        }
    }
}
async fn identities(fixture: &Fixture) -> Vec<Identity> {
    tokio::time::timeout(BOUND, async {
        loop {
            if let Some(values) = std::fs::read(fixture.workspace.join("tree-pids.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<Identity>>(&bytes).ok())
            {
                return values;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn gone(values: &[Identity]) {
    for identity in values {
        assert_ne!(
            started(identity.pid).as_deref(),
            Some(identity.started.as_str()),
            "owned snapshot process survived final receipt: {}",
            identity.pid
        );
    }
}

#[tokio::test]
async fn snapshot_worker_confirms_private_file_clean_environment_and_null_helper_stdin() {
    let fixture = Fixture::new(
        r#"
assert all(os.environ.get(name) is None for name in ['BUZZ_PRIVATE_KEY','BUZZ_AUTH_TAG','OPENAI_API_KEY','ANTHROPIC_API_KEY','AWS_SECRET_ACCESS_KEY','SNAPSHOT_FIXTURE_AMBIENT_LEAK'])
assert pathlib.Path.cwd()==root
assert pathlib.Path(os.environ['TMPDIR'])==out.parent
assert sys.stdin.buffer.read(1)==b''
sys.stderr.write('DO_NOT_RETURN_DIAGNOSTIC')
print(receipt(artifact()),flush=True)
"#,
    );
    let mut worker = Worker::start(&fixture, 60);
    worker.started().await;
    let ended = worker.finish(0).await;
    assert_eq!(ended["reason"], Value::Null);
    assert_eq!(
        ended["receipt"],
        json!({"version":1,"scanned":true,"repositories":1,"bytes":21})
    );
    assert_eq!(
        std::fs::read(&fixture.output).unwrap(),
        b"scanned fixture bytes"
    );
    assert_eq!(
        std::fs::metadata(&fixture.output)
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
}

#[tokio::test]
async fn snapshot_worker_refuses_malformed_oversized_unscanned_and_nonprivate_results() {
    for body in [
        "n=artifact();print(receipt(n,scanned=False))",
        "n=artifact();print(receipt(n,version=2))",
        "n=artifact();print(receipt(n,repositories=65))",
        "n=artifact();print(receipt(n,bytes=0))",
        "n=artifact();print(receipt(n,bytes=n+1))",
        "n=artifact();out.chmod(0o644);print(receipt(n))",
        "n=artifact();print(receipt(n,unknown='DO_NOT_RETURN_PRIVATE_FIELD'))",
        "artifact();print('DO_NOT_RETURN_MALFORMED_METADATA')",
        "n=artifact();print(receipt(n)+' '*4097)",
        "artifact();sys.stderr.write('scan'+' '*33);sys.exit(1)",
    ] {
        let fixture = Fixture::new(body);
        let mut worker = Worker::start(&fixture, 60);
        worker.started().await;
        let ended = worker.finish(1).await;
        assert_eq!(ended["receipt"], Value::Null, "{body}");
        assert_eq!(ended["reason"], "archive", "{body}");
        assert!(
            !fixture.output.exists(),
            "refused artifact was retained: {body}"
        );
    }
}

#[tokio::test]
async fn snapshot_worker_preserves_existing_output_and_rejects_output_symlinks() {
    let fixture = Fixture::new("print(receipt(artifact()),flush=True)");
    std::fs::write(&fixture.output, b"existing caller file").unwrap();
    let mut worker = Worker::start(&fixture, 60);
    assert_eq!(
        tokio::time::timeout(BOUND, worker.child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(2)
    );
    assert_eq!(
        std::fs::read(&fixture.output).unwrap(),
        b"existing caller file"
    );

    let fixture = Fixture::new(
        r#"
target=root/'outside';target.write_bytes(b'preserve outside fixture')
os.symlink(target,out)
print(receipt(24),flush=True)
"#,
    );
    let mut worker = Worker::start(&fixture, 60);
    worker.started().await;
    assert_eq!(worker.finish(1).await["reason"], "archive");
    assert!(std::fs::symlink_metadata(&fixture.output).is_err());
    assert_eq!(
        std::fs::read(fixture.workspace.join("outside")).unwrap(),
        b"preserve outside fixture"
    );
}

#[tokio::test]
async fn snapshot_worker_reaps_detached_jobs_before_success_and_preserves_siblings() {
    let fixture = Fixture::new(&format!("{TREE}\nprint(receipt(artifact()),flush=True)"));
    let mut sibling = Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut worker = Worker::start(&fixture, 60);
    worker.started().await;
    let pids = identities(&fixture).await;
    let _guards: Vec<_> = pids.iter().filter_map(OwnedPid::capture).collect();
    let ended = worker.next().await;
    assert_eq!(ended["type"], "ended");
    assert!(!ended["receipt"].is_null());
    gone(&pids); // This must hold at the receipt, before waiting on the worker.
    assert!(tokio::time::timeout(BOUND, worker.child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert!(sibling.try_wait().unwrap().is_none());
    sibling.kill().await.unwrap();
    sibling.wait().await.unwrap();
}

#[tokio::test]
async fn snapshot_worker_expiry_eof_and_term_stop_helper_trees_without_leaks() {
    for mode in ["expiry", "eof", "term"] {
        let fixture = Fixture::new(&format!("{TREE}\nartifact();time.sleep(60)"));
        let mut worker = Worker::start(&fixture, if mode == "expiry" { 3 } else { 60 });
        worker.started().await;
        let pids = identities(&fixture).await;
        let _guards: Vec<_> = pids.iter().filter_map(OwnedPid::capture).collect();
        match mode {
            "eof" => {
                worker.input.take();
            }
            "term" => nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(worker.child.id().unwrap() as i32),
                nix::sys::signal::Signal::SIGTERM,
            )
            .unwrap(),
            _ => assert!(worker.input.is_some(), "expiry must not be caused by EOF"),
        }
        let ended = worker.next().await;
        assert_eq!(ended["type"], "ended");
        assert_eq!(ended["receipt"], Value::Null);
        assert_eq!(ended["reason"], "authority");
        gone(&pids);
        assert_eq!(
            tokio::time::timeout(BOUND, worker.child.wait())
                .await
                .unwrap()
                .unwrap()
                .code(),
            Some(1)
        );
        assert!(!fixture.output.exists());
    }
}

#[tokio::test]
async fn snapshot_worker_parent_death_cleans_up_while_grandparent_keeps_stdin_open() {
    let fixture = Fixture::new(&format!("{TREE}\nartifact();time.sleep(60)"));
    let driver = r#"
import json,os,pathlib,select,signal,subprocess,sys,time
binary,workspace,output,helper=sys.argv[1:];root=pathlib.Path(workspace)
ri,wi=os.pipe();ro,wo=os.pipe();worker=None;worker_fd=None;pids=[];fds=[]
code='import os,subprocess,sys; p=subprocess.Popen([sys.argv[1],"sandbox-snapshot-worker",sys.argv[2],sys.argv[3],str(int(__import__("time").time())+60),"1048576",sys.argv[4]],stdin=int(sys.argv[5]),stdout=int(sys.argv[6]),stderr=subprocess.DEVNULL,env={"PATH":os.environ["PATH"],"HOME":sys.argv[2]});print(p.pid,flush=True);sys.stdin.buffer.read()'
parent=subprocess.Popen([sys.executable,'-c',code,binary,workspace,output,helper,str(ri),str(wo)],pass_fds=(ri,wo),stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
os.close(ri);os.close(wo);buffer=bytearray()
def read_kind(kind,seconds=8):
    until=time.monotonic()+seconds
    while time.monotonic()<until:
        if select.select([ro],[],[],.05)[0]:
            chunk=os.read(ro,4096)
            if not chunk:raise AssertionError('worker EOF without receipt')
            buffer.extend(chunk);assert len(buffer)<=8192
            while b'\n' in buffer:
                line,_,rest=buffer.partition(b'\n');buffer[:]=rest
                value=json.loads(line)
                if value['type']==kind:return value
    raise AssertionError('worker receipt deadline')
try:
    assert select.select([parent.stdout],[],[],5)[0]
    worker=int(parent.stdout.readline());worker_fd=os.pidfd_open(worker)
    (root/'parent-death-worker.json').write_text(json.dumps({'worker':worker}))
    read_kind('started')
    until=time.monotonic()+5
    while time.monotonic()<until:
        try:pids=json.loads((root/'tree-pids.json').read_text());break
        except (FileNotFoundError,ValueError):time.sleep(.01)
    assert len(pids)==2
    for item in pids:fds.append(os.pidfd_open(item['pid']))
    (root/'parent-death-worker.json').write_text(json.dumps({'worker':worker,'pids':pids}))
    parent.kill();parent.wait(timeout=3)
    ended=read_kind('ended')
    assert ended['receipt'] is None and ended['reason']=='authority'
    assert all(not pathlib.Path('/proc',str(item['pid'])).exists() for item in pids)
    assert not pathlib.Path(output).exists()
    print(json.dumps({'worker':worker,'kept_stdin_open':True,'ended_after_cleanup':True}))
finally:
    # Retain wi until AFTER the receipt: parent-death, not EOF, must stop it.
    os.close(wi);os.close(ro)
    if parent.poll() is None:parent.kill();parent.wait()
    if worker_fd is not None:
        try:signal.pidfd_send_signal(worker_fd,signal.SIGTERM)
        except ProcessLookupError:pass
        os.close(worker_fd)
    for fd in fds:
        try:signal.pidfd_send_signal(fd,signal.SIGKILL)
        except ProcessLookupError:pass
        os.close(fd)
"#;
    let mut command = Command::new("python3");
    command
        .args(["-c", driver])
        .arg(env!("CARGO_BIN_EXE_buzz-acp"))
        .arg(&fixture.workspace)
        .arg(&fixture.output)
        .arg(&fixture.helper)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", fixture.root.path())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(24), command.output())
        .await
        .unwrap()
        .unwrap();
    let saved: Value = serde_json::from_slice(
        &std::fs::read(fixture.workspace.join("parent-death-worker.json")).unwrap(),
    )
    .unwrap();
    let worker = saved["worker"].as_i64().unwrap() as i32;
    let status = tokio::time::timeout(BOUND, async {
        loop {
            match nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(worker),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ) {
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                result => break result.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "parent-death probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        status,
        nix::sys::wait::WaitStatus::Exited(nix::unistd::Pid::from_raw(worker), 1)
    );
    let proof: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proof["kept_stdin_open"], true);
    assert_eq!(proof["ended_after_cleanup"], true);
}

#[tokio::test]
async fn snapshot_worker_preserves_only_fixed_packaged_producer_failure_categories() {
    for (body, reason) in [
        (
            r#"sys.stderr.write('{"error":"scan"}');sys.exit(1)"#,
            "scan",
        ),
        (
            r#"sys.stderr.write('{"error":"budget"}');sys.exit(1)"#,
            "budget",
        ),
        (
            r#"sys.stderr.write('{"error":"DO_NOT_LEAK"}');sys.exit(1)"#,
            "archive",
        ),
        (
            r#"sys.stderr.write('{"error":"scan","path":"x"}');sys.exit(1)"#,
            "archive",
        ),
    ] {
        let fixture = Fixture::new(body);
        let mut worker = Worker::start(&fixture, 60);
        worker.started().await;
        let ended = worker.finish(1).await;
        assert_eq!(ended["receipt"], Value::Null);
        assert_eq!(ended["reason"], reason);
    }
}
