//! Repository Git reads against the local TLS test relay (Git is served
//! over HTTPS only). Skipped unless `BUZZ_TEST_RELAY` points at
//! `.test-relay` and `scripts/test-relay.sh up-tls` has run. Needs `git`.
//!
//! A throwaway repository is announced, read while empty, pushed to with
//! the repository authorization token, then read through buzz-desktop's
//! `project_git` and system Git: history, files, the README, one file and
//! one commit's changes.

use buzz_identity::Identity;
use buzz_relay_client::projects::{EntityFailure, GitRead};
use buzz_relay_client::*;
use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

struct Tls {
    origin: RelayOrigin,
    http: String,
    general: String,
    nsec: String,
    ca: String,
}

fn tls() -> Option<Tls> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let read = |name: &str| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).ok()?).ok()
    };
    let relay = read("tls/relay.json")?;
    let identities = read("identities.json")?;
    // One process per test file: reqwest and Git both trust the test CA.
    // SAFETY: set before this test starts any thread that reads it.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", relay["ca_bundle"].as_str()?);
    }
    Some(Tls {
        origin: RelayOrigin::parse(relay["relay_ws_url"].as_str()?).ok()?,
        http: relay["relay_http_url"].as_str()?.to_string(),
        general: relay["channels"]["general"].as_str()?.to_string(),
        nsec: identities["identities"]["alice"]["nsec"]
            .as_str()?
            .to_string(),
        ca: relay["ca_file"].as_str()?.to_string(),
    })
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_repository_history_files_and_changes() {
    let Some(tls) = tls() else {
        eprintln!("BUZZ_TEST_RELAY or the TLS relay is not set up; skipping");
        return;
    };
    let alice = Arc::new(Identity::from_nsec(&tls.nsec).unwrap());
    let owner = alice.pubkey_hex();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dtag = format!("gpui-git-{nanos}");
    // A repository bound to #general: the binding is the Git ACL.
    let event = EventBuilder::new(Kind::from_u16(30617), "")
        .tags([
            Tag::parse(["d", dtag.as_str()]).unwrap(),
            Tag::parse(["buzz-channel", tls.general.as_str()]).unwrap(),
        ])
        .custom_created_at(Timestamp::now())
        .finalize(&Keys::parse(&tls.nsec).unwrap())
        .unwrap();
    let reply = RelayHttp::new(tls.origin.clone(), alice.clone())
        .post("/events", serde_json::to_string(&event).unwrap())
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(reply["accepted"], true, "{reply}");

    let session = Session::open(
        &tokio::runtime::Handle::current(),
        tls.origin.clone(),
        alice,
        Store::in_memory().unwrap(),
    );
    let projects = session.projects();
    let read = |commit: Option<&str>, path: Option<&str>| GitRead {
        owner: owner.clone(),
        dtag: dtag.clone(),
        commit: commit.map(str::to_string),
        path: path.map(str::to_string),
    };

    // The token authorizes this repository's Git routes; without it the
    // relay refuses.
    let repository = format!("{}/git/{owner}/{dtag}.git", tls.http);
    let token = projects.git_authorization(&repository).unwrap();
    let refs = format!("{repository}/info/refs?service=git-upload-pack");
    let client = reqwest::Client::new();
    let anonymous = client.get(&refs).send().await.unwrap().status();
    let authorized = client
        .get(&refs)
        .header("Authorization", format!("Nostr {token}"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(anonymous.as_u16(), 401, "anonymous");
    assert_eq!(authorized.as_u16(), 200, "authorized");
    assert!(
        projects
            .git_authorization(&format!("https://elsewhere.example/git/{owner}/{dtag}.git"))
            .is_err()
    );

    // An announced repository without content is empty, not missing.
    let empty = projects.read_git(read(None, None)).await.unwrap();
    assert_eq!(empty.head, None);
    assert!(empty.files.is_empty() && empty.readme.is_none());

    // Push two commits with the same token.
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("README.md"), "# Demo\n\nHello from the test.\n").unwrap();
    std::fs::create_dir(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    git(dir, &["add", "."]);
    let identity = [
        "-c",
        "user.name=Alice",
        "-c",
        "user.email=alice@example.com",
    ];
    git(
        dir,
        &[&identity[..], &["commit", "-q", "-m", "First commit"]].concat(),
    );
    let first = git(dir, &["rev-parse", "HEAD"]);
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    git(dir, &["add", "."]);
    git(
        dir,
        &[&identity[..], &["commit", "-q", "-m", "Say hi"]].concat(),
    );
    let second = git(dir, &["rev-parse", "HEAD"]);
    let header = format!("http.extraHeader=Authorization: Nostr {token}");
    let ca = format!("http.sslCAInfo={}", tls.ca);
    git(
        dir,
        &["-c", &header, "-c", &ca, "push", "-q", &repository, "main"],
    );

    let snapshot = projects.read_git(read(None, None)).await.unwrap();
    assert_eq!(snapshot.head.as_deref(), Some(second.as_str()));
    let subjects: Vec<&str> = snapshot
        .commits
        .iter()
        .map(|c| c.subject.as_str())
        .collect();
    assert_eq!(subjects, ["Say hi", "First commit"]);
    assert_eq!(snapshot.commits[0].author, "Alice");
    let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["README.md", "src/main.rs"]);
    assert_eq!(
        snapshot.readme.as_deref(),
        Some("# Demo\n\nHello from the test.\n")
    );
    assert!(snapshot.diff.is_none() && snapshot.file.is_none());

    let file = projects
        .read_git(read(None, Some("src/main.rs")))
        .await
        .unwrap();
    let file = file.file.unwrap();
    assert_eq!(file.path, "src/main.rs");
    assert!(file.content.unwrap().contains("println!(\"hi\")"));

    let commit = projects.read_git(read(Some(&first), None)).await.unwrap();
    assert_eq!(commit.head.as_deref(), Some(first.as_str()));
    let diff = commit.diff.unwrap();
    assert!(
        diff.contains("First commit") && diff.contains("+fn main() {}"),
        "{diff}"
    );

    // A path the tree does not have is not found.
    assert_eq!(
        projects
            .read_git(read(None, Some("missing.txt")))
            .await
            .unwrap_err(),
        EntityFailure::NotFound
    );
    // A repository nobody announced cannot be read.
    assert_eq!(
        projects
            .read_git(GitRead {
                dtag: "never-announced".into(),
                ..read(None, None)
            })
            .await
            .unwrap_err(),
        EntityFailure::Unavailable
    );

    // Leave nothing announced behind.
    let address = format!("30617:{owner}:{dtag}");
    let deletion = EventBuilder::new(Kind::from_u16(5), "")
        .tags([Tag::parse(["a", address.as_str()]).unwrap()])
        .custom_created_at(Timestamp::now())
        .finalize(&Keys::parse(&tls.nsec).unwrap())
        .unwrap();
    RelayHttp::new(
        tls.origin.clone(),
        Arc::new(Identity::from_nsec(&tls.nsec).unwrap()),
    )
    .post("/events", serde_json::to_string(&deletion).unwrap())
    .await
    .unwrap();
}
