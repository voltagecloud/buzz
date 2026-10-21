//! buzz-app's `setup.test.ts` for Claude and Codex, on a stand-in tool.

use super::testing::{Script, Scripted, fetch, run};
use super::*;

const SUM: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A tool whose status the test sets, and whose `use` step makes the
/// release it switched to the one in use.
struct Fake {
    status: Arc<Mutex<Status>>,
    calls: super::testing::Calls,
    checksum: Option<&'static str>,
    passes: bool,
    min: Option<&'static str>,
}

impl Tool for Fake {
    fn name(&self) -> &'static str {
        "Fake"
    }
    fn origin(&self) -> &'static str {
        "fake.example"
    }
    fn pin(&self) -> &'static str {
        "BUZZ_FAKE_VERSION"
    }
    fn env(&self) -> BTreeMap<String, Option<String>> {
        BTreeMap::new()
    }
    fn min_version(&self) -> Option<&'static str> {
        self.min
    }
    fn check(&self, _: Spawner) -> BoxFuture<'static, Status> {
        let switched = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|(line, _)| line.strip_prefix("use ").map(ready));
        let status = switched.unwrap_or_else(|| self.status.lock().unwrap().clone());
        Box::pin(async move { status })
    }
    fn login(&self) -> (&'static str, Vec<String>) {
        ("login", vec![])
    }
    fn target(&self, uname: &str) -> Option<&'static str> {
        (uname == "Linux x86_64").then_some("linux-x64")
    }
    fn latest(&self, fetch: Fetch) -> BoxFuture<'static, Result<Release, String>> {
        Box::pin(async move {
            Ok(Release {
                version: fetch_text(&fetch, "fake.example", "https://fake.example/latest".into())
                    .await?
                    .trim()
                    .to_string(),
                data: Value::Null,
            })
        })
    }
    fn checksum(
        &self,
        _: Fetch,
        _: &Release,
        _: &str,
    ) -> BoxFuture<'static, Result<Option<String>, String>> {
        let checksum = self.checksum.map(str::to_string);
        Box::pin(async move { Ok(checksum) })
    }
    fn test(&self, spawn: Spawner, version: String) -> BoxFuture<'static, Result<(), String>> {
        let passes = self.passes;
        Box::pin(async move {
            // The test runs the downloaded release, not the one in use.
            run_short(&spawn, "fake", &["--version"], BTreeMap::new()).await?;
            if passes {
                Ok(())
            } else {
                Err(format!("{version} did not list its models"))
            }
        })
    }
}

struct Fixture {
    setup: Setup,
    status: Arc<Mutex<Status>>,
    spawn: Scripted,
}

fn fixture(status: Status, latest: &'static str, passes: bool) -> Fixture {
    fixture_with(status, latest, passes, Some(SUM), None)
}

fn fixture_with(
    status: Status,
    latest: &'static str,
    passes: bool,
    checksum: Option<&'static str>,
    min: Option<&'static str>,
) -> Fixture {
    let status = Arc::new(Mutex::new(status));
    let spawn = Scripted::new(&[("platform", run("Linux x86_64\n", 0))]);
    let tool = Fake {
        status: status.clone(),
        calls: spawn.calls.clone(),
        checksum,
        passes,
        min,
    };
    let setup = Setup::new(
        Arc::new(tool),
        Some(Arc::new(spawn.clone())),
        fetch(&[("https://fake.example/latest", 200, latest)]),
    );
    Fixture {
        setup,
        status,
        spawn,
    }
}

fn ready(version: &str) -> Status {
    Status::Ready {
        version: version.into(),
        account: None,
    }
}

#[tokio::test]
async fn installs_the_latest_release_once_it_passes_its_test() {
    let fixture = fixture(ready("1.0.0"), "1.1.0\n", true);

    fixture.setup.run(Step::Install).await;
    assert_eq!(
        fixture.spawn.lines(),
        [
            "platform",
            &format!("install 1.1.0 linux-x64 {SUM}"),
            "fake --version",
            "use 1.1.0"
        ]
    );
    // Only the test ran the new release before it was in use.
    let pinned: Vec<_> = fixture
        .spawn
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, pinned)| pinned.clone())
        .collect();
    assert_eq!(pinned, [None, None, Some("1.1.0".into()), None]);
    let state = fixture.setup.snapshot();
    assert_eq!(
        state.output,
        "Downloading Fake 1.1.0…\nTesting Fake 1.1.0…\nFake 1.1.0 is installed.\n"
    );
    assert_eq!(state.status, Some(ready("1.1.0")));
    assert_eq!((state.running, state.notice), (None, None));
}

#[tokio::test]
async fn keeps_the_copy_in_use_when_a_new_release_fails_its_test() {
    let fixture = fixture(ready("1.0.0"), "1.1.0", false);
    fixture.setup.update(true).await;
    let state = fixture.setup.snapshot();
    assert!(!fixture.spawn.lines().contains(&"use 1.1.0".to_string()));
    assert_eq!(
        state.notice.as_deref(),
        Some("Fake 1.1.0 failed its test: 1.1.0 did not list its models. Buzz keeps using 1.0.0.")
    );
    assert!(
        state
            .output
            .ends_with("\nFake 1.1.0 failed its test: 1.1.0 did not list its models")
    );
    assert_eq!(state.status, Some(ready("1.0.0")));
    // Not downloaded again until asked for.
    let before = fixture.spawn.lines().len();
    fixture.setup.update(true).await;
    assert_eq!(fixture.spawn.lines().len(), before);
    fixture.setup.run(Step::Install).await;
    assert!(fixture.spawn.lines().len() > before);
}

#[tokio::test]
async fn does_nothing_when_the_copy_is_already_the_latest() {
    let fixture = fixture(ready("1.1.0"), "1.1.0", true);
    fixture.setup.update(true).await;
    assert!(fixture.spawn.lines().is_empty());
    assert_eq!(fixture.setup.snapshot().output, "");
}

#[tokio::test]
async fn refuses_a_release_older_than_buzz_needs() {
    let fixture = fixture_with(Status::Missing, "2.1.100", true, Some(SUM), Some("2.1.121"));
    fixture.setup.run(Step::Install).await;
    assert!(fixture.spawn.lines().is_empty());
    assert_eq!(
        fixture.setup.snapshot().output,
        "\nFake 2.1.100 is older than Buzz needs (2.1.121)."
    );
}

#[tokio::test]
async fn refuses_a_release_without_a_checksum_for_this_computer() {
    let unlisted = fixture_with(Status::Missing, "1.1.0", true, Some("nope"), None);
    unlisted.setup.run(Step::Install).await;
    assert_eq!(unlisted.spawn.lines(), ["platform"]);
    assert_eq!(
        unlisted.setup.snapshot().output,
        "\nFake 1.1.0 lists no checksum for linux-x64"
    );
    let fixture = fixture(Status::Missing, "latest", true);
    fixture.setup.run(Step::Install).await;
    assert_eq!(
        fixture.setup.snapshot().output,
        "\nfake.example did not name a release"
    );
}

#[tokio::test]
async fn waits_for_an_agent_before_the_first_download() {
    let fixture = fixture(Status::Missing, "1.1.0", true);
    fixture.setup.update(false).await;
    assert!(fixture.spawn.lines().is_empty());
    assert_eq!(fixture.setup.snapshot().status, Some(Status::Missing));
}

#[tokio::test]
async fn holds_a_delivery_while_it_downloads_the_first_copy() {
    let fixture = fixture(Status::Missing, "1.1.0", true);

    let open = fixture.spawn.gate("install");
    let update = tokio::spawn(fixture.setup.update(true));
    let installed = tokio::spawn({
        let setup = fixture.setup.clone();
        async move { setup.installed().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!installed.is_finished());
    open.send(()).unwrap();
    installed.await.unwrap();
    update.await.unwrap();
    assert_eq!(fixture.setup.snapshot().status, Some(ready("1.1.0")));
}

#[tokio::test]
async fn does_not_hold_deliveries_while_it_updates_a_copy_in_use() {
    let fixture = fixture(ready("1.0.0"), "1.1.0", true);
    let _open = fixture.spawn.gate("install");
    fixture
        .spawn
        .set(&format!("install 1.1.0 linux-x64 {SUM}"), Script::Hang);
    let update = tokio::spawn(fixture.setup.update(true));
    tokio::time::timeout(Duration::from_secs(1), fixture.setup.installed())
        .await
        .expect("a copy in use holds nothing");
    fixture.setup.cancel().await;
    update.abort();
}

#[tokio::test]
async fn holds_a_delivery_that_arrives_while_it_first_looks_for_its_copy() {
    let fixture = fixture(Status::Missing, "1.1.0", true);

    // Claimed before it first runs: no status yet, and the delivery waits.
    let update = fixture.setup.update(true);
    let installed = tokio::spawn({
        let setup = fixture.setup.clone();
        async move { setup.installed().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!installed.is_finished());
    update.await;
    installed.await.unwrap();
    assert_eq!(fixture.setup.snapshot().status, Some(ready("1.1.0")));
}

#[tokio::test]
async fn tries_the_first_download_again_when_a_mention_finds_no_copy() {
    let fixture = fixture(Status::Missing, "1.1.0", true);
    // The first download failed.
    fixture
        .spawn
        .set(&format!("install 1.1.0 linux-x64 {SUM}"), run("", 1));
    fixture.setup.update(true).await;
    assert_eq!(fixture.setup.snapshot().status, Some(Status::Missing));
    assert!(
        fixture
            .setup
            .snapshot()
            .output
            .ends_with("\nDownload ended (1).")
    );
    fixture
        .spawn
        .set(&format!("install 1.1.0 linux-x64 {SUM}"), run("", 0));

    fixture.setup.installed().await;
    assert_eq!(fixture.setup.snapshot().status, Some(ready("1.1.0")));
}

#[tokio::test]
async fn runs_sign_in_shows_its_output_and_checks_again_after() {
    let fixture = fixture(
        Status::SignedOut {
            version: "1.0.0".into(),
        },
        "1.0.0",
        true,
    );
    fixture.spawn.set("login", run("Opening browser…\n", 0));
    let status = fixture.status.clone();
    *status.lock().unwrap() = ready("1.0.0");
    fixture.setup.run(Step::Login).await;
    let state = fixture.setup.snapshot();
    assert_eq!(state.running, None);
    assert_eq!(state.output, "Opening browser…\n");
    assert_eq!(state.status, Some(ready("1.0.0")));
    assert_eq!(fixture.spawn.lines(), ["login"]);
}

#[tokio::test]
async fn stops_a_step_cancelled_before_its_process_was_returned() {
    let fixture = fixture(
        Status::SignedOut {
            version: "1.0.0".into(),
        },
        "1.0.0",
        true,
    );
    fixture.spawn.set("login", Script::Hang);
    let open = fixture.spawn.gate("login");
    let running = tokio::spawn({
        let setup = fixture.setup.clone();
        async move { setup.run(Step::Login).await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(fixture.setup.snapshot().running, Some(Step::Login));
    fixture.setup.cancel().await;
    open.send(()).unwrap();
    running.await.unwrap();
    assert!(fixture.spawn.killed.load(Ordering::SeqCst));
    let state = fixture.setup.snapshot();
    assert_eq!(state.running, None);
    assert_eq!(state.output, "\nSign-in ended (stopped).");
}

#[tokio::test]
async fn explains_that_it_needs_a_host() {
    let tool = Fake {
        status: Arc::new(Mutex::new(Status::Missing)),
        calls: Arc::default(),
        checksum: None,
        passes: true,
        min: None,
    };
    let setup = Setup::new(Arc::new(tool), None, fetch(&[]));
    setup.check().await;
    assert_eq!(
        setup.snapshot().status,
        Some(Status::Error {
            message: "Fake agents run only in the desktop app".into()
        })
    );
}

#[test]
fn orders_versions() {
    use std::cmp::Ordering::*;
    assert_eq!(compare_versions("2.1.121", "2.1.121"), Equal);
    assert_eq!(compare_versions("2.1.99", "2.1.121"), Less);
    assert_eq!(compare_versions("10.0.0", "9.9.9"), Greater);
    assert!(release_name("0.162.1"));
    assert!(!release_name("1.2"));
    assert!(!release_name("1.2.x"));
}
