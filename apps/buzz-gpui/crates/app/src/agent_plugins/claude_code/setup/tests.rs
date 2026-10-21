//! buzz-app's `setup.test.ts`: what is Claude's own. The shared install
//! and hold flow is tested in `managed`.

use super::*;
use crate::agent_plugins::managed::testing::{Script, Scripted, fetch, run};

#[tokio::test]
async fn finds_claude_missing_when_buzz_has_no_copy() {
    let spawn: Spawner = Arc::new(Scripted::new(&[(
        "claude --version",
        Script::Run {
            stdout: "",
            stderr: "Claude is not installed\n",
            code: 127,
        },
    )]));
    assert_eq!(check_claude(&spawn).await, ClaudeStatus::Missing);
    // A host from before Buzz's copy cannot start the copy's steps.
    let update = "Update this host to run Claude agents";
    let old: Spawner = Arc::new(Scripted::new(&[("claude --version", Script::Fail(update))]));
    assert_eq!(
        check_claude(&old).await,
        ClaudeStatus::Error {
            message: update.into()
        }
    );
}

#[tokio::test]
async fn finds_a_copy_older_than_buzz_needs_outdated() {
    let spawn: Spawner = Arc::new(Scripted::new(&[(
        "claude --version",
        run("2.1.3 (Claude Code)\n", 0),
    )]));
    assert_eq!(
        check_claude(&spawn).await,
        ClaudeStatus::Outdated {
            version: "2.1.3".into()
        }
    );
}

#[tokio::test]
async fn reports_the_version_and_whether_it_is_signed_in() {
    let signed_out: Spawner = Arc::new(Scripted::new(&[
        ("claude --version", run("2.1.130 (Claude Code)\n", 0)),
        ("claude auth status --json", run("{\"loggedIn\":false}", 1)),
    ]));
    assert_eq!(
        check_claude(&signed_out).await,
        ClaudeStatus::SignedOut {
            version: "2.1.130".into()
        }
    );
    let ready: Spawner = Arc::new(Scripted::new(&[
        ("claude --version", run("2.1.130 (Claude Code)\n", 0)),
        (
            "claude auth status --json",
            run(
                "{\"loggedIn\":true,\"email\":\"a@example.com\",\"orgName\":\"Example\"}",
                0,
            ),
        ),
    ]));
    assert_eq!(
        check_claude(&ready).await,
        ClaudeStatus::Ready {
            version: "2.1.130".into(),
            account: Some("a@example.com · Example".into())
        }
    );
}

#[tokio::test]
async fn reads_the_latest_release_and_its_checksum_for_this_computer() {
    let sum = "ab".repeat(32);
    let manifest = format!(
        "{{\"platforms\":{{\"linux-x64\":{{\"checksum\":\"{sum}\"}},\"darwin-arm64\":{{}}}}}}"
    );
    let fetch = fetch(&[
        (&format!("{RELEASES}/latest"), 200, "2.1.140\n"),
        (&format!("{RELEASES}/2.1.140/manifest.json"), 200, &manifest),
    ]);
    let release = Claude.latest(fetch.clone()).await.unwrap();
    assert_eq!(release.version, "2.1.140");
    assert_eq!(Claude.target("Linux x86_64"), Some("linux-x64"));
    assert_eq!(Claude.target("Linux armv7l"), None);
    assert_eq!(
        Claude
            .checksum(fetch.clone(), &release, "linux-x64")
            .await
            .unwrap(),
        Some(sum)
    );
    assert_eq!(
        Claude
            .checksum(fetch, &release, "darwin-arm64")
            .await
            .unwrap(),
        None
    );
    let down = super::managed::testing::fetch(&[]);
    assert_eq!(
        Claude.latest(down).await.err(),
        Some("downloads.claude.ai answered 404".into())
    );
}

#[test]
fn claude_keeps_itself_where_buzz_put_it() {
    assert_eq!(env().get("DISABLE_AUTOUPDATER"), Some(&Some("1".into())));
    assert_eq!(
        Claude.login(),
        ("claude", vec!["auth".into(), "login".into()])
    );
}
