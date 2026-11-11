//! One claimed export: confirmed containment, durable upload intent, then delivery.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, ensure, Result};
use buzz_sdk::sandbox_attach::snapshot::{self, upload, Failure, Grant, MAX_BLOB_BYTES};
use nostr::{Event, Timestamp};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{timeout, timeout_at, Instant};

use super::{client::ContainmentError, server::Config, snapshot_client::Job, snapshot_upload};
use crate::relay::RestClient;

const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(15);
const FILE_TIMEOUT: Duration = Duration::from_secs(15);

/// The owner of relay I/O must save exact metadata durably before confirming it.
pub(super) enum Step {
    /// Historical recovery metadata must also be serviced after authority ends.
    Metadata {
        event: Event,
        confirmed: oneshot::Sender<Result<()>>,
    },
    /// Publish only after a fresh authority check; never put this in a generic outbox.
    Ready {
        event: Event,
        confirmed: oneshot::Sender<Result<()>>,
    },
}

/// Public metadata for the final audit; local errors never include worker diagnostics.
pub(super) struct Completion {
    pub(super) bytes: u64,
    pub(super) reason: Option<Failure>,
}

fn failed(bytes: u64, reason: Failure) -> Completion {
    Completion {
        bytes,
        reason: Some(reason),
    }
}

fn allowed_now(allowed: &watch::Receiver<bool>, grant: &Grant) -> bool {
    *allowed.borrow()
        && allowed.has_changed().is_ok()
        && Timestamp::now().as_secs() < grant.expires_at()
}

async fn revoked(mut allowed: watch::Receiver<bool>, expiry: Instant) {
    // Sender loss is an authority failure even when the last cached value was true.
    let _ = timeout_at(expiry, allowed.wait_for(|value| !*value)).await;
}

async fn checkpoint(steps: &mpsc::Sender<Step>, event: Event, ready: bool) -> Result<()> {
    let (confirmed, receipt) = oneshot::channel();
    let step = if ready {
        Step::Ready { event, confirmed }
    } else {
        Step::Metadata { event, confirmed }
    };
    timeout(CHECKPOINT_TIMEOUT, async {
        steps
            .send(step)
            .await
            .map_err(|_| anyhow!("snapshot metadata handler ended"))?;
        receipt
            .await
            .map_err(|_| anyhow!("snapshot metadata confirmation was lost"))?
    })
    .await
    .map_err(|_| anyhow!("snapshot metadata confirmation timed out"))?
}

async fn archive(path: &Path, expected: u64) -> Result<Vec<u8>> {
    timeout(FILE_TIMEOUT, async {
        ensure!(
            (1..=MAX_BLOB_BYTES as u64).contains(&expected),
            "snapshot archive exceeds bound"
        );
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)
            .await
            .map_err(|_| anyhow!("snapshot archive unavailable"))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|_| anyhow!("snapshot archive metadata unavailable"))?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == nix::unistd::geteuid().as_raw()
                && metadata.permissions().mode() & 0o077 == 0
                && metadata.len() == expected,
            "snapshot archive must match its private worker receipt"
        );
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(expected as usize + 1)
            .map_err(|_| anyhow!("snapshot archive memory budget unavailable"))?;
        file.take(expected + 1).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() as u64 == expected,
            "snapshot archive changed after worker receipt"
        );
        Ok(bytes)
    })
    .await
    .map_err(|_| anyhow!("snapshot archive read timed out"))?
}

async fn ciphertext(path: &Path, bytes: &[u8]) -> Result<()> {
    timeout(FILE_TIMEOUT, async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)
            .await
            .map_err(|_| anyhow!("snapshot ciphertext staging unavailable"))?;
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("snapshot ciphertext staging timed out"))?
}

async fn cleanup(directory: &Path) -> Result<()> {
    timeout(FILE_TIMEOUT, async {
        for name in ["archive.tar.gz", "ciphertext.bin"] {
            match tokio::fs::remove_file(directory.join(name)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(anyhow!("snapshot artifact cleanup remains pending")),
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("snapshot artifact cleanup remains pending"))?
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    config: &Config,
    rest: &RestClient,
    grant: &Grant,
    accepted: &Event,
    directory: &Path,
    helper: &Path,
    work_bytes: u64,
    allowed: watch::Receiver<bool>,
    steps: &mpsc::Sender<Step>,
) -> Result<Completion> {
    ensure!(
        grant.scope() == config.scope
            && grant.event().pubkey == config.owner
            && grant.scope().agent == config.keys.public_key()
            && rest.keys.public_key() == config.keys.public_key(),
        "snapshot runner must match its immutable sandbox scope and owner"
    );
    if !allowed_now(&allowed, grant) {
        return Ok(failed(0, Failure::Authority));
    }
    let expiry = Instant::now()
        + Duration::from_secs(
            grant
                .expires_at()
                .saturating_sub(Timestamp::now().as_secs()),
        );
    let archive_file = directory.join("archive.tar.gz");
    // Startup owns a spawned child before Ready. Never cancel/drop this future:
    // the client must confirm cleanup itself if its five-second startup fails.
    let mut job = match Job::start(
        &config.executable,
        helper,
        &config.workspace,
        &archive_file,
        grant.expires_at(),
        work_bytes,
    )
    .await
    {
        Ok(job) => job,
        Err(error) if error.downcast_ref::<ContainmentError>().is_some() => return Err(error),
        Err(_) => return Ok(failed(0, Failure::Archive)),
    };
    let produced = tokio::select! {
        biased;
        _=revoked(allowed.clone(),expiry)=>None,
        completion=job.next()=>Some(completion),
    };
    // next() is cancellation safe, and close() retains its absolute cleanup
    // deadline. No encryption/upload starts before this confirms the full tree.
    job.close().await?;
    let receipt = match produced {
        None => return Ok(failed(0, Failure::Authority)),
        Some(Err(error)) if error.downcast_ref::<ContainmentError>().is_some() => {
            return Err(error)
        }
        Some(Err(_)) => return Ok(failed(0, Failure::Archive)),
        Some(Ok(completion)) => match completion.receipt {
            Some(receipt) => receipt,
            None => return Ok(failed(0, completion.reason.unwrap_or(Failure::Archive))),
        },
    };
    if !allowed_now(&allowed, grant) {
        return Ok(failed(0, Failure::Authority));
    }
    let plain = tokio::select! {
        biased;
        _=revoked(allowed.clone(),expiry)=>return Ok(failed(0,Failure::Authority)),
        result=archive(&archive_file,receipt.bytes)=>match result {
            Ok(bytes)=>bytes,
            Err(_)=>return Ok(failed(0,Failure::Archive)),
        },
    };
    let approved = grant.clone();
    // The bounded CPU operation is joined even after revocation. Dropping a
    // blocking task could leave plaintext work alive after the runner finishes.
    let sealed = match tokio::task::spawn_blocking(move || snapshot::seal(&approved, &plain)).await
    {
        Ok(Ok(sealed)) => sealed,
        _ => return Ok(failed(0, Failure::Archive)),
    };
    let bytes = sealed.ciphertext().len() as u64;
    if !allowed_now(&allowed, grant) {
        return Ok(failed(bytes, Failure::Authority));
    }
    let ciphertext_file = directory.join("ciphertext.bin");
    if ciphertext(&ciphertext_file, sealed.ciphertext())
        .await
        .is_err()
    {
        return Ok(failed(bytes, Failure::Archive));
    }
    if !allowed_now(&allowed, grant) {
        return Ok(failed(bytes, Failure::Authority));
    }
    let intent = upload::intent(&config.keys, grant, accepted, &sealed)?;
    let planned = upload::Upload::parse(&intent, grant.scope(), config.owner)?;
    // This exact signed plan must have both a durable local record and confirmed
    // relay acceptance before a single byte may be sent to Blossom.
    checkpoint(steps, intent, false).await?;
    if !allowed_now(&allowed, grant) {
        return Ok(failed(bytes, Failure::Authority));
    }
    let uploaded = tokio::select! {
        biased;
        _=revoked(allowed.clone(),expiry)=>None,
        result=snapshot_upload::put(rest,&ciphertext_file,sealed.hash(),bytes)=>Some(result),
    };
    let state = if matches!(uploaded, Some(Ok(()))) {
        upload::State::Uploaded
    } else {
        upload::State::Unconfirmed
    };
    // An interrupted PUT can already have committed. Historical outcome records
    // must survive lost authority and may never release the encrypted blob key.
    let outcome = upload::outcome(&config.keys, &planned, state)?;
    checkpoint(steps, outcome, false).await?;
    if !allowed_now(&allowed, grant) || uploaded.is_none() {
        return Ok(failed(bytes, Failure::Authority));
    }
    if !matches!(uploaded, Some(Ok(()))) {
        return Ok(failed(bytes, Failure::Upload));
    }
    let ready = snapshot::result(&config.keys, grant, &sealed)?;
    // The main loop performs another fresh authority check at the actual write,
    // and must not persist/replay this key-bearing event in a generic outbox.
    checkpoint(steps, ready, true).await?;
    if !allowed_now(&allowed, grant) {
        return Ok(failed(bytes, Failure::Authority));
    }
    Ok(Completion {
        bytes,
        reason: None,
    })
}

/// Execute one already claimed export; the caller must await it through cleanup.
/// The caller also owns a durable directory-cleanup intent before invocation.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    config: &Config,
    rest: &RestClient,
    grant: &Grant,
    accepted: &Event,
    directory: &Path,
    helper: &Path,
    work_bytes: u64,
    allowed: watch::Receiver<bool>,
    steps: mpsc::Sender<Step>,
) -> Result<Completion> {
    let result = execute(
        config, rest, grant, accepted, directory, helper, work_bytes, allowed, &steps,
    )
    .await;
    // A fatal containment error requires whole-harness shutdown. Preserve its
    // journal and files; an unconfirmed worker might still be accessing them.
    if result
        .as_ref()
        .err()
        .is_some_and(|error| error.downcast_ref::<ContainmentError>().is_some())
    {
        return result;
    }
    cleanup(directory).await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, routing::put, Json, Router};
    use buzz_sdk::sandbox_attach::Scope;
    use nostr::hashes::{sha256, Hash};
    use nostr::{EventId, Keys};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::sync::Notify;
    use uuid::Uuid;

    const TEST_BOUND: Duration = Duration::from_secs(8);
    const PLAIN: &[u8] = b"snapshot-export-fixture";

    struct Fixture {
        _root: tempfile::TempDir,
        config: Config,
        rest: RestClient,
        requester: Keys,
        grant: Grant,
        accepted: Event,
        directory: PathBuf,
        calls: Arc<AtomicUsize>,
        body: Arc<Mutex<Vec<u8>>>,
        arrived: Arc<Notify>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new(hold_response: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let executable = root.path().join("worker.py");
            // Deliberately replace only the already-tested native containment
            // boundary. This drives the production Job IPC and export runner.
            std::fs::write(&executable, r#"#!/usr/bin/env python3
import json,os,sys
assert sys.argv[1]=='sandbox-snapshot-worker'
fd=os.open(sys.argv[3],os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
with os.fdopen(fd,'wb') as file:file.write(b'snapshot-export-fixture')
print(json.dumps({'type':'started'}),flush=True)
print(json.dumps({'type':'ended','receipt':{'version':1,'scanned':True,'repositories':1,'bytes':23},'reason':None}),flush=True)
"#).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let workspace = root.path().join("workspace");
            let directory = root.path().join("job");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::create_dir(&directory).unwrap();
            let owner = Keys::generate();
            let agent = Keys::generate();
            let requester = owner.clone();
            let scope = Scope {
                channel: Uuid::new_v4(),
                thread: EventId::all_zeros(),
                agent: agent.public_key(),
            };
            let now = Timestamp::now().as_secs();
            let request = snapshot::request(&requester, scope, Uuid::new_v4(), now + 120).unwrap();
            let pending = snapshot::Pending::parse(&request, now).unwrap();
            let grant = Grant::parse(
                &snapshot::approve(&owner, &pending, now + 120).unwrap(),
                scope,
                now,
            )
            .unwrap();
            let accepted = snapshot::audit(&agent, &grant, "accepted", 0, None).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let body = Arc::new(Mutex::new(Vec::new()));
            let arrived = Arc::new(Notify::new());
            let (count, stored, notified) = (calls.clone(), body.clone(), arrived.clone());
            let app = Router::new().route("/upload", put(move |bytes: Bytes| {
                let (count, stored, notified) = (count.clone(), stored.clone(), notified.clone());
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    *stored.lock().unwrap() = bytes.to_vec();
                    notified.notify_one();
                    if hold_response { tokio::time::sleep(Duration::from_secs(10)).await; }
                    Json(json!({"sha256":sha256::Hash::hash(&bytes).to_string(),"size":bytes.len()}))
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let config = Config {
                relay: url.replacen("http://", "ws://", 1),
                keys: agent.clone(),
                auth: None,
                owner: owner.public_key(),
                scope,
                workspace,
                executable,
                journal: root.path().join("journal"),
            };
            Self {
                _root: root,
                config,
                rest: RestClient {
                    http: reqwest::Client::new(),
                    base_url: url,
                    keys: agent,
                    auth_tag_json: None,
                },
                requester,
                grant,
                accepted,
                directory,
                calls,
                body,
                arrived,
                server,
            }
        }

        async fn metadata(
            &self,
            updates: &mut mpsc::Receiver<Step>,
            state: upload::State,
        ) -> oneshot::Sender<Result<()>> {
            let Step::Metadata { event, confirmed } = updates.recv().await.unwrap() else {
                panic!("expected durable upload metadata before delivery");
            };
            let metadata =
                upload::Upload::parse(&event, self.config.scope, self.config.owner).unwrap();
            assert_eq!(metadata.state(), state);
            assert_eq!(metadata.claim(), self.accepted.id);
            confirmed
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    #[tokio::test]
    async fn runner_requires_confirmed_intent_and_outcome_before_upload_and_ready() {
        let fixture = Fixture::new(false).await;
        let (_authority, allowed) = watch::channel(true);
        let (steps, mut updates) = mpsc::channel(1);
        let runner = run(
            &fixture.config,
            &fixture.rest,
            &fixture.grant,
            &fixture.accepted,
            &fixture.directory,
            Path::new("/fixture-helper"),
            1024 * 1024,
            allowed,
            steps,
        );
        let observer = async {
            let intent = fixture.metadata(&mut updates, upload::State::Planned).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(
                fixture.calls.load(Ordering::SeqCst),
                0,
                "PUT preceded confirmed intent"
            );
            intent.send(Ok(())).unwrap();
            let outcome = fixture
                .metadata(&mut updates, upload::State::Uploaded)
                .await;
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
            assert!(
                updates.try_recv().is_err(),
                "Ready preceded confirmed outcome"
            );
            outcome.send(Ok(())).unwrap();
            let Step::Ready { event, confirmed } = updates.recv().await.unwrap() else {
                panic!("expected recipient-encrypted Ready after confirmed upload");
            };
            let receipt = snapshot::receipt(
                &fixture.requester,
                &fixture.grant,
                &event,
                Timestamp::now().as_secs(),
            )
            .unwrap();
            assert_eq!(
                receipt
                    .open(&fixture.body.lock().unwrap(), Timestamp::now().as_secs())
                    .unwrap(),
                PLAIN
            );
            confirmed.send(Ok(())).unwrap();
        };
        let (done, ()) = timeout(TEST_BOUND, async { tokio::join!(runner, observer) })
            .await
            .unwrap();
        let done = done.unwrap();
        assert!(done.reason.is_none());
        assert_eq!(done.bytes, fixture.body.lock().unwrap().len() as u64);
        assert!(!fixture.directory.join("archive.tar.gz").exists());
        assert!(!fixture.directory.join("ciphertext.bin").exists());
    }

    #[tokio::test]
    async fn revocation_records_upload_outcome_without_releasing_ready() {
        for hold_response in [false, true] {
            let fixture = Fixture::new(hold_response).await;
            let (authority, allowed) = watch::channel(true);
            let (steps, mut updates) = mpsc::channel(1);
            let runner = run(
                &fixture.config,
                &fixture.rest,
                &fixture.grant,
                &fixture.accepted,
                &fixture.directory,
                Path::new("/fixture-helper"),
                1024 * 1024,
                allowed,
                steps,
            );
            let observer = async {
                fixture
                    .metadata(&mut updates, upload::State::Planned)
                    .await
                    .send(Ok(()))
                    .unwrap();
                let outcome = if hold_response {
                    fixture.arrived.notified().await;
                    authority.send(false).unwrap();
                    fixture
                        .metadata(&mut updates, upload::State::Unconfirmed)
                        .await
                } else {
                    let confirmed = fixture
                        .metadata(&mut updates, upload::State::Uploaded)
                        .await;
                    authority.send(false).unwrap();
                    confirmed
                };
                // A committed request can lose its response: keep durable recovery
                // metadata even though current authority now forbids key delivery.
                outcome.send(Ok(())).unwrap();
                assert!(
                    updates.recv().await.is_none(),
                    "revocation still released Ready"
                );
            };
            let (done, ()) = timeout(TEST_BOUND, async { tokio::join!(runner, observer) })
                .await
                .unwrap();
            assert!(matches!(done.unwrap().reason, Some(Failure::Authority)));
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
            assert!(!fixture.directory.join("archive.tar.gz").exists());
            assert!(!fixture.directory.join("ciphertext.bin").exists());
        }
    }
}
