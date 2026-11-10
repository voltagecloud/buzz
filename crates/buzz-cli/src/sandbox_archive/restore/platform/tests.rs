use super::*;
use crate::sandbox_archive::format::tests as fixture;
use serde_json::json;

fn archive() -> Vec<u8> {
    let (_source, manifest, bundle) = fixture::fixture(false);
    fixture::packed(fixture::entries(&manifest, &bundle))
}
fn no_staging(store: &Path) {
    assert!(!fs::read_dir(store).unwrap().any(|entry| {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with(".buzz-snapshot-work-") || name.starts_with(".buzz-snapshot-ready-")
    }));
}
#[tokio::test]
async fn real_git_restore_keeps_history_and_diffs_while_omitting_private_working_paths() {
    for sha256 in [false, true] {
        let (source, mut manifest, _) = fixture::fixture(sha256);
        let previous = manifest["repositories"][0]["head"]
            .as_str()
            .unwrap()
            .to_owned();
        fs::write(source.path().join("deleted.txt"), "gone from worktree\n").unwrap();
        fixture::git(source.path(), &["add", "deleted.txt"]);
        fixture::git(
            source.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "deletion fixture",
            ],
        );
        let bundle = source.path().join("history.bundle");
        fs::remove_file(&bundle).unwrap();
        fixture::git(
            source.path(),
            &[
                "bundle",
                "create",
                bundle.to_str().unwrap(),
                "--all",
                "HEAD",
            ],
        );
        manifest["repositories"][0]["head"] = json!(String::from_utf8(fixture::git(
            source.path(),
            &["rev-parse", "HEAD"]
        ))
        .unwrap()
        .trim());
        manifest["repositories"][0]["files"] = json!(["tracked.txt", "inside-link"]);
        let mut items = fixture::entries(&manifest, &fs::read(bundle).unwrap());
        items[2].0.set_mode(0o755);
        items[2].0.set_cksum();
        let mut link = fixture::header("snapshot/repositories/0/files/inside-link", b'2', 0);
        link.set_link_name("tracked.txt").unwrap();
        link.set_cksum();
        items.push((link, vec![]));
        let archive = fixture::packed(items);
        let store = tempfile::tempdir().unwrap();
        let destination = store.path().join("mirror");
        let restored = extract(
            archive,
            &destination,
            store.path(),
            RestoreBudget::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let repo = &restored.repositories[0];
        assert_eq!(restored.directory, destination);
        assert_eq!(fs::read(repo.join("tracked.txt")).unwrap(), b"dirty\n");
        assert!(!repo.join(".env.example").exists());
        assert!(
            String::from_utf8(fixture::git(repo, &["show", "HEAD:.env.example"]))
                .unwrap()
                .contains("TEMPLATE=public")
        );
        assert_eq!(
            fixture::git(repo, &["diff", "--name-status"]),
            b"D\tdeleted.txt\nM\ttracked.txt\n"
        );
        assert_eq!(
            fixture::git(repo, &["rev-parse", "refs/heads/local-only"]),
            format!("{previous}\n").as_bytes()
        );
        for path in [
            repo.clone(),
            repo.join("tracked.txt"),
            repo.join(".git/index"),
        ] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o222, 0);
        }
        assert_eq!(
            fs::read_link(repo.join("inside-link")).unwrap(),
            Path::new("tracked.txt")
        );
        assert_eq!(fs::read(repo.join("inside-link")).unwrap(), b"dirty\n");
        assert_eq!(
            fs::metadata(repo.join("tracked.txt")).unwrap().mode() & 0o777,
            0o500
        );
        no_staging(store.path());
        cleanup(&destination).unwrap();
    }
}
#[tokio::test]
async fn capacity_existing_store_and_fresh_destination_are_enforced() {
    let store = tempfile::tempdir().unwrap();
    let destination = store.path().join("mirror");
    let small = RestoreBudget {
        work_bytes: 1024 * 1024,
        store_bytes: 128 * 1024 * 1024,
    };
    assert!(extract(
        archive(),
        &destination,
        store.path(),
        small,
        CancellationToken::new()
    )
    .await
    .is_err());
    assert!(!destination.exists());
    no_staging(store.path());
    fs::write(store.path().join("existing"), vec![1u8; 8 * 1024 * 1024]).unwrap();
    let tight = RestoreBudget {
        work_bytes: 32 * 1024 * 1024,
        store_bytes: 32 * 1024 * 1024,
    };
    assert!(extract(
        archive(),
        &destination,
        store.path(),
        tight,
        CancellationToken::new()
    )
    .await
    .is_err());
    no_staging(store.path());
    fs::create_dir(&destination).unwrap();
    assert!(extract(
        archive(),
        &destination,
        store.path(),
        RestoreBudget::default(),
        CancellationToken::new()
    )
    .await
    .is_err());
    assert!(destination.is_dir());
    assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
}
#[tokio::test]
async fn atomic_noreplace_closes_destination_creation_race() {
    let store = tempfile::tempdir().unwrap();
    let destination = store.path().join("mirror");
    let token = CancellationToken::new();
    let stage = prepare(
        archive(),
        destination.clone(),
        store.path().into(),
        RestoreBudget::default(),
        token.clone(),
    )
    .unwrap();
    import(&stage, &token).await.unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(finish(
        stage,
        destination.clone(),
        store.path().into(),
        RestoreBudget::default(),
        token
    )
    .is_err());
    assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
    no_staging(store.path());
}
#[tokio::test]
async fn real_shallow_bundle_needs_its_declared_missing_parent_boundary() {
    let (source, mut manifest, _) = fixture::fixture(false);
    fs::write(source.path().join("tracked.txt"), "second\n").unwrap();
    fixture::git(source.path(), &["add", "tracked.txt"]);
    fixture::git(
        source.path(),
        &["-c", "commit.gpgsign=false", "commit", "-m", "second"],
    );
    let cloned = tempfile::tempdir().unwrap();
    fixture::git(
        cloned.path(),
        &[
            "clone",
            "--template=",
            "--depth=1",
            &format!("file://{}", source.path().display()),
            "clone",
        ],
    );
    let repo = cloned.path().join("clone");
    let head = String::from_utf8(fixture::git(&repo, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    let bundle = repo.join("history.bundle");
    fixture::git(
        &repo,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "--all",
            "HEAD",
        ],
    );
    let bundle = fs::read(bundle).unwrap();
    manifest["repositories"][0]["head"] = json!(head);
    manifest["repositories"][0]["shallow"] = json!([head]);
    for declared in [false, true] {
        let store = tempfile::tempdir().unwrap();
        let destination = store.path().join("mirror");
        let mut current = manifest.clone();
        if !declared {
            current["repositories"][0]["shallow"] = json!([]);
        }
        let result = extract(
            fixture::packed(fixture::entries(&current, &bundle)),
            &destination,
            store.path(),
            RestoreBudget::default(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(result.is_ok(), declared);
        no_staging(store.path());
        if declared {
            assert!(destination.join("workspace/.git/shallow").is_file());
            cleanup(&destination).unwrap();
        }
    }
}
#[tokio::test]
async fn cancellation_cleans_staging_at_prepare_import_and_publish_boundaries() {
    let store = tempfile::tempdir().unwrap();
    let destination = store.path().join("mirror");
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        extract(
            archive(),
            &destination,
            store.path(),
            RestoreBudget::default(),
            token
        )
        .await,
        Err(Error::Cancelled)
    ));
    for imported in [false, true] {
        let token = CancellationToken::new();
        let stage = prepare(
            archive(),
            destination.clone(),
            store.path().into(),
            RestoreBudget::default(),
            token.clone(),
        )
        .unwrap();
        if imported {
            import(&stage, &token).await.unwrap();
        }
        token.cancel();
        if imported {
            assert!(matches!(
                finish(
                    stage,
                    destination.clone(),
                    store.path().into(),
                    RestoreBudget::default(),
                    token
                ),
                Err(Error::Cancelled)
            ));
        } else {
            assert!(matches!(
                import(&stage, &token).await,
                Err(Error::Cancelled)
            ));
            cleanup(stage.directory.path()).unwrap();
            cleanup(stage.ready.path()).unwrap();
        }
        assert!(!destination.exists());
        no_staging(store.path());
    }
}
#[test]
fn git_process_boundary_is_exercised_in_isolated_child_environments() {
    let temp = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    for mode in ["cancel", "exit", "deny", "flood"] {
        let script = temp.path().join("git");
        let marker = temp.path().join("child");
        let _ = fs::remove_file(&marker);
        let body = match mode {
            "deny" => "exit 126\n".to_owned(),
            "flood" => "printf '%08192d' 0\n".to_owned(),
            _ => format!(
                "while :; do :; done >/dev/null 2>&1 &\necho $! > '{}'\n{}\n",
                marker.display(),
                if mode == "cancel" { "wait" } else { "exit 0" }
            ),
        };
        fs::write(&script, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let output = std::process::Command::new(&executable)
            .args([
                "--exact",
                "sandbox_archive::restore::platform::tests::git_process_driver",
                "--ignored",
            ])
            .env("PATH", temp.path())
            .env("BUZZ_TEST_GIT_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if marker.exists() {
            let pid = fs::read_to_string(&marker).unwrap();
            let path = format!("/proc/{}/stat", pid.trim());
            #[cfg(target_os = "linux")]
            if let Ok(stat) = fs::read_to_string(path) {
                assert_eq!(
                    stat.split(')').nth(1).unwrap().split_whitespace().next(),
                    Some("Z")
                );
            }
        }
    }
}
#[tokio::test]
#[ignore = "isolated child fixture for the automatic Git process boundary parent test"]
async fn git_process_driver() {
    let mode = std::env::var("BUZZ_TEST_GIT_MODE").unwrap();
    let token = CancellationToken::new();
    if mode == "cancel" {
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
    }
    let result = git(
        &std::env::current_dir().unwrap(),
        &["fixture"],
        None,
        4096,
        Duration::from_secs(3),
        &token,
    )
    .await;
    match mode.as_str() {
        "cancel" => assert!(matches!(result, Err(Error::Cancelled))),
        "exit" => {
            assert!(result.is_ok());
        }
        "deny" | "flood" => assert!(result.is_err()),
        _ => panic!("unknown fixture"),
    }
}

#[tokio::test]
#[ignore = "requires exact hardened producer archive in BUZZ_SNAPSHOT_ARCHIVE_FIXTURE"]
async fn exact_packaged_archive_restores_history_and_working_diff() {
    let bytes = fs::read(std::env::var("BUZZ_SNAPSHOT_ARCHIVE_FIXTURE").unwrap()).unwrap();
    let store = tempfile::tempdir().unwrap();
    let destination = store.path().join("mirror");
    let restored = extract(
        bytes,
        &destination,
        store.path(),
        RestoreBudget::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let repo = &restored.repositories[0];
    assert_eq!(fs::read(repo.join("tracked.txt")).unwrap(), b"dirty\n");
    assert!(!repo.join(".env.example").exists());
    assert_eq!(
        fs::read_link(repo.join("inside-link")).unwrap(),
        Path::new("tracked.txt")
    );
    assert_eq!(
        fixture::git(repo, &["diff", "--name-status"]),
        b"M\ttracked.txt\n"
    );
    assert_eq!(
        fixture::git(repo, &["rev-parse", "refs/heads/local-only"]),
        fixture::git(repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o500);
    no_staging(store.path());
    cleanup(&destination).unwrap();
}

#[test]
fn empty_deep_repository_directories_are_reserved_before_writes() {
    let (_source, mut manifest, bundle) = fixture::fixture(false);
    let mut records = Vec::new();
    let mut items = Vec::new();
    for index in 0..64 {
        let mut repo = manifest["repositories"][0].clone();
        repo["path"] = json!(format!("repo-{index}/{}", "a/".repeat(126)) + "a");
        repo["files"] = json!([]);
        records.push(repo);
        items.push((
            fixture::header(
                &format!("snapshot/repositories/{index}/history.bundle"),
                b'0',
                bundle.len() as u64,
            ),
            bundle.clone(),
        ));
    }
    manifest["repositories"] = json!(records);
    let data = serde_json::to_vec(&manifest).unwrap();
    items.push((
        fixture::header("snapshot/manifest.json", b'0', data.len() as u64),
        data,
    ));
    let bytes = fixture::packed(items);
    let token = CancellationToken::new();
    let plan = format::inspect_archive(&bytes, &token).unwrap();
    assert!(projection(&plan, 4096).unwrap() > 48 * 1024 * 1024);
    let store = tempfile::tempdir().unwrap();
    let budget = RestoreBudget {
        work_bytes: 48 * 1024 * 1024,
        store_bytes: 128 * 1024 * 1024,
    };
    assert!(prepare(
        bytes,
        store.path().join("mirror"),
        store.path().into(),
        budget,
        token
    )
    .is_err());
    no_staging(store.path());
}

#[tokio::test]
async fn malformed_bundle_ref_headers_cannot_create_undeclared_refs() {
    let (_source, manifest, bundle) = fixture::fixture(false);
    let end = bundle.windows(2).position(|s| s == b"\n\n").unwrap();
    let header = std::str::from_utf8(&bundle[..end]).unwrap();
    let oid = manifest["repositories"][0]["head"].as_str().unwrap();
    for (value, name) in [
        ("0".repeat(40), format!("refs/heads/injected {oid}")),
        (oid.to_owned(), "refs/heads/bad..name".into()),
        ("0".repeat(40), "refs/heads/null".into()),
    ] {
        let mut malformed = format!("{header}\n{value} {name}\n\n").into_bytes();
        malformed.extend_from_slice(&bundle[end + 2..]);
        let store = tempfile::tempdir().unwrap();
        let destination = store.path().join("mirror");
        let result = extract(
            fixture::packed(fixture::entries(&manifest, &malformed)),
            &destination,
            store.path(),
            RestoreBudget::default(),
            CancellationToken::new(),
        )
        .await;
        if name == "refs/heads/null" {
            assert!(matches!(
                result,
                Err(Error::Invalid("null snapshot reference object"))
            ));
        } else if name.contains(' ') {
            assert!(matches!(result, Err(Error::Bundle(_))));
        } else {
            assert!(matches!(result, Err(Error::Git(_))));
        }
        assert!(!destination.exists());
        no_staging(store.path());
    }
}

#[tokio::test]
async fn nonowner_execute_bits_do_not_invent_git_mode_changes() {
    let (source, manifest, bundle) = fixture::fixture(false);
    fs::set_permissions(
        source.path().join("tracked.txt"),
        fs::Permissions::from_mode(0o654),
    )
    .unwrap();
    assert!(fixture::git(source.path(), &["diff", "--name-status"]).is_empty());
    let mut items = fixture::entries(&manifest, &bundle);
    items[2].1 = b"clean\n".to_vec();
    items[2].0.set_mode(0o654);
    items[2].0.set_cksum();
    let store = tempfile::tempdir().unwrap();
    let destination = store.path().join("mirror");
    let restored = extract(
        fixture::packed(items),
        &destination,
        store.path(),
        RestoreBudget::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let repo = &restored.repositories[0];
    assert!(fixture::git(repo, &["diff", "--name-status"]).is_empty());
    assert_eq!(
        fs::metadata(repo.join("tracked.txt")).unwrap().mode() & 0o777,
        0o400
    );
    cleanup(&destination).unwrap();
}
