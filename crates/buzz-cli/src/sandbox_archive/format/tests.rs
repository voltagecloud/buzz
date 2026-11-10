use super::*;
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Value};
use std::{fs, io::Write, process::Command};

fn git(repo: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn fixture(sha256: bool) -> (tempfile::TempDir, Value, Vec<u8>) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    git(
        repo,
        &[
            "init",
            "--template=",
            if sha256 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            },
        ],
    );
    fs::write(repo.join("tracked.txt"), "clean\n").unwrap();
    fs::write(repo.join(".env.example"), "TEMPLATE=public\n").unwrap();
    git(repo, &["add", "."]);
    git(
        repo,
        &["-c", "commit.gpgsign=false", "commit", "-m", "fixture"],
    );
    git(repo, &["branch", "local-only"]);
    let bundle = repo.join("history.bundle");
    git(
        repo,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "--all",
            "HEAD",
        ],
    );
    let head = String::from_utf8(git(repo, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    let manifest = json!({"version":1,"snapshot":true,"scanned":true,"repositories":[{"path":"workspace","head":head,"base":"bundle","branch":"main","shallow":[],"files":["tracked.txt"],"excluded":[".env.example"],"deleted":[]}]});
    let bytes = fs::read(bundle).unwrap();
    (temp, manifest, bytes)
}
fn header(path: &str, kind: u8, size: u64) -> tar::Header {
    let mut h = tar::Header::new_ustar();
    h.set_size(size);
    h.set_mode(0o644);
    h.set_entry_type(tar::EntryType::new(kind));
    assert!(path.len() <= 100);
    h.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
    h.set_cksum();
    h
}
fn packed(entries: Vec<(tar::Header, Vec<u8>)>) -> Vec<u8> {
    let gzip = GzEncoder::new(Vec::new(), Compression::fast());
    let mut tar = tar::Builder::new(gzip);
    for (h, data) in entries {
        tar.append(&h, data.as_slice()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}
fn entries(manifest: &Value, bundle: &[u8]) -> Vec<(tar::Header, Vec<u8>)> {
    let data = serde_json::to_vec(manifest).unwrap();
    vec![
        (
            header("snapshot/manifest.json", b'0', data.len() as u64),
            data,
        ),
        (
            header(
                "snapshot/repositories/0/history.bundle",
                b'0',
                bundle.len() as u64,
            ),
            bundle.to_vec(),
        ),
        (
            header("snapshot/repositories/0/files/tracked.txt", b'0', 6),
            b"dirty\n".to_vec(),
        ),
    ]
}
fn inspect(data: &[u8]) -> Result<ArchivePlan, Error> {
    inspect_archive(data, &CancellationToken::new())
}
fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut n = body.len() + 1;
    loop {
        let next = body.len() + n.to_string().len();
        if next == n {
            return format!("{n}{body}").into_bytes();
        }
        n = next;
    }
}
#[test]
fn real_bundles_and_policy_fixture_cover_full_history_and_copied_exclusions() {
    for sha256 in [false, true] {
        let (_temp, manifest, bundle) = fixture(sha256);
        let plan = inspect(&packed(entries(&manifest, &bundle))).unwrap();
        assert_eq!(plan.repositories().collect::<Vec<_>>(), ["workspace"]);
        assert_eq!(plan.files(), 1);
        assert_eq!(plan.symlinks(), 0);
        assert!(
            plan.objects() >= 4
                && plan.expanded_bytes() > 20
                && plan.uncompressed_bytes() > bundle.len() as u64
        );
        assert!(plan.inventory[&0]
            .refs
            .iter()
            .any(|(_, name)| name == "refs/heads/local-only"));
    }
    let cases: Vec<(String, bool)> = serde_json::from_str(include_str!(
        "../../../../../voltage/images/buzz-agent/snapshot-policy.json"
    ))
    .unwrap();
    for (path, expected) in cases {
        assert_eq!(excluded(&safe(&path).unwrap()), expected, "{path}");
    }
}
#[test]
fn metadata_types_duplicates_and_path_guards_are_bound_to_real_archive_iteration() {
    let (_temp, manifest, bundle) = fixture(false);
    let good = entries(&manifest, &bundle);
    for path in ["../outside", "/outside", "a/../b", "a\\b", "a//b", "a:b"] {
        let mut bad = good.clone();
        bad[2] = (
            header(&format!("snapshot/repositories/0/files/{path}"), b'0', 0),
            vec![],
        );
        let mut altered = manifest.clone();
        altered["repositories"][0]["files"] = json!([path]);
        bad[0].1 = serde_json::to_vec(&altered).unwrap();
        let size = bad[0].1.len() as u64;
        bad[0].0.set_size(size);
        bad[0].0.set_cksum();
        assert!(inspect(&packed(bad)).is_err(), "{path}");
    }
    for ty in *b"1346SLKg" {
        let mut bad = good.clone();
        bad[2] = (
            header("snapshot/repositories/0/files/tracked.txt", ty, 0),
            vec![],
        );
        assert!(inspect(&packed(bad)).is_err(), "type{ty}");
    }
    let mut duplicate = good.clone();
    duplicate.push(good[2].clone());
    assert!(inspect(&packed(duplicate)).is_err());
    let mut unscanned = manifest.clone();
    unscanned["scanned"] = json!(false);
    assert!(inspect(&packed(entries(&unscanned, &bundle))).is_err());
    let mut private = manifest.clone();
    private["repositories"][0]["files"] = json!([".Git/config"]);
    let mut private_entries = entries(&private, &bundle);
    private_entries[2].0 = header("snapshot/repositories/0/files/.Git/config", b'0', 6);
    assert!(inspect(&packed(private_entries)).is_err());
    let mut undeclared = good.clone();
    undeclared.push((header("snapshot/unused", b'5', 0), vec![]));
    assert!(inspect(&packed(undeclared)).is_err());
}
#[test]
fn pax_is_bounded_before_payload_allocation_and_accepts_python_long_paths() {
    let (_temp, mut manifest, bundle) = fixture(false);
    let name = format!("{}file.txt", "long/".repeat(24));
    manifest["repositories"][0]["files"] = json!([name]);
    let mut items = entries(&manifest, &bundle);
    items.pop();
    let path = format!("snapshot/repositories/0/files/{name}");
    let extension = pax_record("path", &path);
    items.push((
        header("././@PaxHeader", b'x', extension.len() as u64),
        extension,
    ));
    items.push((header("placeholder", b'0', 6), b"dirty\n".to_vec()));
    assert_eq!(inspect(&packed(items)).unwrap().files(), 1);
    // A sparse compressed 1MiB extension must be refused at the 16KiB declaration.
    let mut bad = entries(&manifest, &bundle);
    bad.push((header("pax", b'x', 1024 * 1024), vec![0; 1024 * 1024]));
    assert!(matches!(
        inspect(&packed(bad)),
        Err(Error::Invalid("snapshot PAX payload exceeds budget"))
    ));
    let mut bad = entries(&manifest, &bundle);
    let metadata = pax_record("SCHILY.xattr.security.capability", "bad");
    bad.push((header("pax", b'x', metadata.len() as u64), metadata));
    assert!(inspect(&packed(bad)).is_err());
}
#[test]
fn symlinks_are_scoped_and_cycles_and_file_ancestors_are_refused() {
    let (_temp, mut manifest, bundle) = fixture(false);
    manifest["repositories"][0]["files"] = json!(["tracked.txt", "link"]);
    for target in ["tracked.txt", "../outside", "/outside", "link"] {
        let mut items = entries(&manifest, &bundle);
        let mut h = header("snapshot/repositories/0/files/link", b'2', 0);
        h.set_link_name(target).unwrap();
        h.set_cksum();
        items.push((h, vec![]));
        assert_eq!(
            inspect(&packed(items)).is_ok(),
            target == "tracked.txt",
            "{target}"
        );
    }
    manifest["repositories"][0]["files"] = json!(["tracked.txt", "tracked.txt/child"]);
    let mut items = entries(&manifest, &bundle);
    items.push((
        header("snapshot/repositories/0/files/tracked.txt/child", b'0', 0),
        vec![],
    ));
    assert!(inspect(&packed(items)).is_err());
}
#[test]
fn gzip_tail_truncation_and_cancellation_refuse_without_filesystem_side_effects() {
    let (_temp, manifest, bundle) = fixture(false);
    let good = packed(entries(&manifest, &bundle));
    let mut concatenated = good.clone();
    concatenated.extend_from_slice(&good);
    assert!(inspect(&concatenated).is_err());
    for length in [0, 10, good.len() - 1] {
        assert!(inspect(&good[..length]).is_err());
    }
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        inspect_archive(&good, &token),
        Err(Error::Cancelled)
    ));
    let mut tar = Vec::new();
    {
        let mut decoder = flate2::read::GzDecoder::new(good.as_slice());
        decoder.read_to_end(&mut tar).unwrap();
    }
    tar.extend_from_slice(b"not-zero");
    let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
    gzip.write_all(&tar).unwrap();
    assert!(inspect(&gzip.finish().unwrap()).is_err());
}

#[test]
fn aggregate_expansion_counts_repeated_compressed_bundles() {
    use sha2::{Digest, Sha256};
    let size = 129 * 1024 * 1024usize;
    let mut pack = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
    let mut remainder = size >> 4;
    pack.push(0xb0 | (size as u8 & 15));
    while remainder > 0 {
        let part = (remainder & 127) as u8;
        remainder >>= 7;
        pack.push(part | if remainder > 0 { 128 } else { 0 });
    }
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
    for _ in 0..size / 1024 {
        zlib.write_all(&[b'x'; 1024]).unwrap();
    }
    pack.extend(zlib.finish().unwrap());
    pack.extend_from_slice(&Sha256::digest(&pack));
    let head = "0".repeat(64);
    let mut bundle =
        format!("# v3 git bundle\n@object-format=sha256\n{head} HEAD\n\n").into_bytes();
    bundle.extend(pack);
    let record = json!({"path":"one","head":head,"base":"bundle","branch":"","shallow":[],"files":[],"excluded":[],"deleted":[]});
    let mut second = record.clone();
    second["path"] = json!("two");
    let manifest =
        json!({"version":1,"snapshot":true,"scanned":true,"repositories":[record,second]});
    let mut items = entries(&manifest, &bundle);
    items.pop();
    items.push((
        header(
            "snapshot/repositories/1/history.bundle",
            b'0',
            bundle.len() as u64,
        ),
        bundle,
    ));
    assert!(matches!(
        inspect(&packed(items)),
        Err(Error::Invalid("snapshot aggregate expansion exceeded"))
    ));
}

#[test]
fn dotdot_is_resolved_after_symlink_expansion() {
    let (_temp, mut manifest, bundle) = fixture(false);
    manifest["repositories"][0]["files"] = json!(["tracked.txt", "pivot", "up", "escape"]);
    let mut items = entries(&manifest, &bundle);
    for (name, target) in [
        ("pivot", "."),
        ("up", "pivot/.."),
        ("escape", "up/../outside"),
    ] {
        let mut h = header(&format!("snapshot/repositories/0/files/{name}"), b'2', 0);
        h.set_link_name(target).unwrap();
        h.set_cksum();
        items.push((h, vec![]));
    }
    assert!(matches!(
        inspect(&packed(items)),
        Err(Error::Invalid("snapshot link escapes repository"))
    ));
}

#[test]
#[ignore = "requires exact packaged producer archive in BUZZ_SNAPSHOT_ARCHIVE_FIXTURE"]
fn exact_packaged_producer_archive() {
    let path = std::env::var("BUZZ_SNAPSHOT_ARCHIVE_FIXTURE").unwrap();
    let plan = inspect(&fs::read(path).unwrap()).unwrap();
    assert_eq!(plan.repositories().collect::<Vec<_>>(), ["workspace"]);
    assert_eq!(plan.files(), 3);
    assert_eq!(plan.symlinks(), 1);
    assert!(plan.inventory[&0]
        .refs
        .iter()
        .any(|(_, name)| name == "refs/heads/local-only"));
}
