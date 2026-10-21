//! buzz-app's `setup.test.ts`: what is Codex's own. The shared install
//! and hold flow is tested in `managed`.

use super::*;
use crate::agent_plugins::managed::testing::{Script, Scripted, fetch, run};

#[tokio::test]
async fn finds_codex_missing_when_buzz_has_no_copy() {
    let spawn: Spawner = Arc::new(Scripted::new(&[(
        "codex --version",
        Script::Run {
            stdout: "",
            stderr: "Codex is not installed\n",
            code: 127,
        },
    )]));
    assert_eq!(check_codex(&spawn).await, CodexStatus::Missing);
}

#[tokio::test]
async fn reports_the_version_and_whether_it_is_signed_in() {
    let signed_out: Spawner = Arc::new(Scripted::new(&[
        ("codex --version", run("codex-cli 0.162.1\n", 0)),
        ("codex login status", run("Not logged in\n", 1)),
    ]));
    assert_eq!(
        check_codex(&signed_out).await,
        CodexStatus::SignedOut {
            version: "0.162.1".into()
        }
    );
    let ready: Spawner = Arc::new(Scripted::new(&[
        ("codex --version", run("codex-cli 0.162.1\n", 0)),
        ("codex login status", run("Logged in using ChatGPT\n", 0)),
    ]));
    assert_eq!(
        check_codex(&ready).await,
        CodexStatus::Ready {
            version: "0.162.1".into(),
            account: None
        }
    );
}

#[tokio::test]
async fn reads_the_latest_release_and_its_checksum_for_this_computer() {
    let sum = "cd".repeat(32);
    let listing = format!(
        "{{\"tag_name\":\"rust-v0.163.0\",\"assets\":[{{\"name\":\"codex-package-x86_64-unknown-linux-musl.tar.gz\",\"digest\":\"sha256:{sum}\"}}]}}"
    );
    let fetch = fetch(&[(&format!("{RELEASES}/channels/latest"), 200, &listing)]);
    let codex = Codex { remote: false };
    let release = codex.latest(fetch.clone()).await.unwrap();
    assert_eq!(release.version, "0.163.0");
    let target = codex.target("Linux x86_64").unwrap();
    assert_eq!(target, "x86_64-unknown-linux-musl");
    assert_eq!(
        codex
            .checksum(fetch.clone(), &release, target)
            .await
            .unwrap(),
        Some(sum)
    );
    // No package for this computer: no checksum.
    assert_eq!(
        codex
            .checksum(fetch, &release, "aarch64-apple-darwin")
            .await
            .unwrap(),
        None
    );
}

#[test]
fn a_remote_host_signs_in_with_a_device_code() {
    assert_eq!(
        Codex { remote: false }.login(),
        ("codex", vec!["login".to_string()])
    );
    assert_eq!(
        Codex { remote: true }.login(),
        (
            "codex",
            vec!["login".to_string(), "--device-auth".to_string()]
        )
    );
}
