use super::*;
use flate2::{write::ZlibEncoder, Compression};
use std::io::Write;
fn object(kind: u8, body: &[u8], declared: u64) -> Vec<u8> {
    let mut size = declared;
    let mut head = ((kind & 7) << 4) | (size as u8 & 15);
    size >>= 4;
    if size > 0 {
        head |= 128;
    }
    let mut bytes = vec![head];
    while size > 0 {
        let mut byte = size as u8 & 127;
        size >>= 7;
        if size > 0 {
            byte |= 128;
        }
        bytes.push(byte);
    }
    if kind == 7 {
        bytes.extend([0u8; 20]);
    }
    let mut compressed = ZlibEncoder::new(Vec::new(), Compression::best());
    compressed.write_all(body).unwrap();
    bytes.extend(compressed.finish().unwrap());
    bytes
}
fn bundle(objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut bytes = format!("# v2 git bundle\n{} refs/heads/main\n\n", "1".repeat(40)).into_bytes();
    bytes.extend(b"PACK");
    bytes.extend(2u32.to_be_bytes());
    bytes.extend((objects.len() as u32).to_be_bytes());
    for object in objects {
        bytes.extend(object);
    }
    bytes.extend([0u8; 20]);
    bytes
}
#[test]
fn pack_preflight_refuses_compressed_expansion_before_git_import() {
    let valid = bundle(vec![object(3, b"hello", 5)]);
    let stats = inspect_bundle(&valid, 100).unwrap();
    assert_eq!(stats.expanded_bytes, 5);
    assert_eq!(stats.objects, 1);
    assert!(
        inspect_bundle(&bundle(vec![object(3, &[0; 101], 101)]), 100).is_err(),
        "declared object size must refuse before decompression"
    );
    assert!(
        inspect_bundle(
            &bundle(vec![object(3, &[0; 60], 60), object(3, &[0; 60], 60)]),
            100
        )
        .is_err(),
        "aggregate expansion must be bounded"
    );
    // Delta stream is tiny, but its declared reconstructed target is 16384 bytes.
    assert!(
        inspect_bundle(&bundle(vec![object(7, &[1, 128, 128, 1], 4)]), 100).is_err(),
        "delta result must be bounded independently of instruction size"
    );
    assert!(inspect_bundle(&bundle(vec![object(3, b"longer than declared", 1)]), 100).is_err());
    assert!(inspect_bundle(&valid[..valid.len() - 1], 100).is_err());
    let count = bundle(vec![object(3, &[], 0); MAX_OBJECTS as usize + 1]);
    assert!(inspect_bundle(&count, 100).is_err());
    let mut thin = valid.clone();
    let header = thin.windows(2).position(|s| s == b"\n\n").unwrap();
    thin.splice(
        header + 1..header + 1,
        format!("-{} missing history\n", "2".repeat(40)).bytes(),
    );
    assert!(inspect_bundle(&thin, 100).is_err());
}
#[test]
fn actual_git_bundle_all_is_accepted_and_object_inventory_matches() {
    use std::process::Command;
    let root = std::env::temp_dir().join(format!("buzz-snapshot-pack-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let run = |args: &[&str]| {
        let result = Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-C",
            ])
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result.stdout
    };
    run(&["init"]);
    std::fs::write(root.join("hello"), "first version\n").unwrap();
    run(&["add", "hello"]);
    run(&["commit", "-m", "first"]);
    std::fs::write(root.join("hello"), "second version\n").unwrap();
    run(&["commit", "-am", "second"]);
    run(&["branch", "local-only", "HEAD~1"]);
    run(&["bundle", "create", "snapshot.bundle", "--all", "HEAD"]);
    let bytes = std::fs::read(root.join("snapshot.bundle")).unwrap();
    let inventory = inspect_bundle(&bytes, MAX_SOURCE_BYTES).unwrap();
    let actual = run(&[
        "cat-file",
        "--batch-all-objects",
        "--batch-check=%(objectsize)",
    ]);
    let total: u64 = String::from_utf8(actual)
        .unwrap()
        .lines()
        .map(|line| line.parse::<u64>().unwrap())
        .sum();
    assert_eq!(inventory.expanded_bytes, total);
    assert_eq!(inventory.hash_bytes, 20);
    assert!(inventory
        .refs
        .iter()
        .any(|(_, name)| name == "refs/heads/local-only"));
    let origin = nostr::Url::from_file_path(&root).unwrap().to_string();
    run(&["clone", "--depth=1", &origin, "shallow"]);
    let shallow = root.join("shallow");
    assert!(!std::fs::read(shallow.join(".git/shallow"))
        .unwrap()
        .is_empty());
    let result = Command::new("git")
        .arg("-C")
        .arg(&shallow)
        .args(["bundle", "create", "shallow.bundle", "--all", "HEAD"])
        .output()
        .unwrap();
    assert!(result.status.success());
    let bytes = std::fs::read(shallow.join("shallow.bundle")).unwrap();
    let shallow_inventory = inspect_bundle(&bytes, MAX_SOURCE_BYTES).unwrap();
    assert!(shallow_inventory.objects < inventory.objects);
    std::fs::remove_dir_all(root).unwrap();
}
