//! Signed plugin releases (buzz-app's NIP-PS, `docs/nips/NIP-PS.md`).
//!
//! buzz-app signs one file holding the built `plugin.js`. A plugin here is
//! a folder, so the signed file lists every file in it with its SHA-256:
//! `plugin.artifact.json`. `plugin.signature.json` is the NIP-PS event
//! (kind 1064, empty content, an `x` tag with the artifact's SHA-256 and a
//! `t` tag naming this format). Hidden entries and the two release files
//! are left out. The folder is checked against the artifact before every
//! start, so a signature vouches for exactly the files that run.
//!
//! The publisher is the verified event's key. A signature says who
//! published a plugin, not that it is safe.

use nostr::prelude::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const ARTIFACT: &str = "plugin.artifact.json";
pub const SIGNATURE: &str = "plugin.signature.json";
/// The `t` tag: this folder format, not buzz-app's `plugin.js` artifact.
pub const FORMAT: &str = "buzz-plugin-folder-v1";
const KIND: u16 = buzz_identity::PLUGIN_RELEASE;

const MAX_FILES: usize = 5_000;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DEPTH: usize = 16;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Artifact {
    version: u32,
    id: String,
    files: Vec<File>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct File {
    path: String,
    size: u64,
    sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The artifact for `folder` as it is now: its exact bytes.
pub fn artifact(folder: &Path, id: &str) -> Result<Vec<u8>, String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![(folder.to_path_buf(), String::new(), 0)];
    while let Some((directory, prefix, depth)) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Could not read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err("A file name in the plugin folder is not UTF-8.".into());
            };
            if name.starts_with('.') || (depth == 0 && (name == ARTIFACT || name == SIGNATURE)) {
                continue;
            }
            let path = format!("{prefix}{name}");
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_symlink() {
                return Err(format!(
                    "{path} is a symlink; a signed plugin has only real files."
                ));
            }
            if kind.is_dir() {
                if depth >= MAX_DEPTH {
                    return Err("The plugin folder is nested too deep to sign.".into());
                }
                stack.push((entry.path(), format!("{path}/"), depth + 1));
                continue;
            }
            let bytes = std::fs::read(entry.path())
                .map_err(|error| format!("Could not read {path}: {error}"))?;
            total += bytes.len() as u64;
            if files.len() >= MAX_FILES || total > MAX_BYTES {
                return Err("The plugin folder is too big to sign.".into());
            }
            files.push(File {
                path,
                size: bytes.len() as u64,
                sha256: sha256(&bytes),
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut bytes = serde_json::to_vec_pretty(&Artifact {
        version: 1,
        id: id.to_owned(),
        files,
    })
    .map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Signs `folder` as it is now with the user's key, writing both release
/// files. Returns the publisher's hex key.
pub fn sign(folder: &Path, id: &str, identity: &buzz_identity::Identity) -> Result<String, String> {
    let bytes = artifact(folder, id)?;
    let event = identity
        .sign_plugin_release(&sha256(&bytes), FORMAT)
        .map_err(|error| error.to_string())?;
    crate::atomic_file::write(&folder.join(ARTIFACT), &bytes).map_err(|error| error.to_string())?;
    let mut signature = serde_json::to_vec_pretty(&event).map_err(|error| error.to_string())?;
    signature.push(b'\n');
    crate::atomic_file::write(&folder.join(SIGNATURE), &signature)
        .map_err(|error| error.to_string())?;
    Ok(event.pubkey.to_hex())
}

/// Who signed `folder`: `None` without a signature, or an error when the
/// signature is not valid or the files changed since signing.
pub fn verify(folder: &Path, id: &str) -> Result<Option<String>, String> {
    let signature = match std::fs::read_to_string(folder.join(SIGNATURE)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not read {SIGNATURE}: {error}")),
    };
    let event: Event = serde_json::from_str(signature.trim())
        .map_err(|_| format!("{SIGNATURE} is not an event."))?;
    let publisher = publisher(&event)?;
    let digest = tag(&event, "x")?;
    let signed = std::fs::read(folder.join(ARTIFACT))
        .map_err(|_| format!("{SIGNATURE} is here but {ARTIFACT} is not."))?;
    if sha256(&signed) != digest {
        return Err(format!("{ARTIFACT} is not the file that was signed."));
    }
    let listed: Artifact =
        serde_json::from_slice(&signed).map_err(|_| format!("{ARTIFACT} is not valid."))?;
    if listed.id != id {
        return Err(format!("It was signed as {}, not {id}.", listed.id));
    }
    if artifact(folder, id)? != signed {
        return Err(
            "Files changed since it was signed. Sign it again, or delete plugin.signature.json."
                .into(),
        );
    }
    Ok(Some(publisher))
}

fn tag<'a>(event: &'a Event, name: &str) -> Result<&'a str, String> {
    let mut values = event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().map(String::as_str) == Some(name));
    match (values.next(), values.next()) {
        (Some([_, value]), None) => Ok(value),
        _ => Err(format!("The signature needs exactly one {name} tag.")),
    }
}

/// Checks the event itself (NIP-PS "Verification and trust").
fn publisher(event: &Event) -> Result<String, String> {
    event
        .verify()
        .map_err(|_| "The signature is not valid.".to_owned())?;
    if event.kind.as_u16() != KIND || !event.content.is_empty() {
        return Err("The signature is not a plugin release.".into());
    }
    if tag(event, "t")? != FORMAT {
        return Err("The signature is for another kind of plugin release.".into());
    }
    let digest = tag(event, "x")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("The signature's digest is not valid.".into());
    }
    Ok(event.pubkey.to_hex())
}

/// A short name for a publisher key, as an npub prefix.
pub fn short_npub(publisher: &str) -> String {
    use nostr::nips::nip19::ToBech32;
    nostr::prelude::PublicKey::from_hex(publisher)
        .ok()
        .and_then(|key| key.to_bech32().ok())
        .map_or_else(
            || publisher.chars().take(12).collect(),
            |npub| format!("{}…{}", &npub[..12], &npub[npub.len() - 6..]),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn folder() -> std::path::PathBuf {
        let folder = std::env::temp_dir().join(format!("buzz-release-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(folder.join("lib")).unwrap();
        std::fs::create_dir_all(folder.join(".cache")).unwrap();
        std::fs::write(folder.join("plugin.py"), "print('hi')").unwrap();
        std::fs::write(folder.join("lib/util.py"), "x = 1").unwrap();
        std::fs::write(folder.join(".cache/state"), "ignored").unwrap();
        folder
    }

    #[test]
    fn a_signed_folder_verifies_until_a_file_changes() {
        let folder = folder();
        assert_eq!(verify(&folder, "a.b"), Ok(None));
        let identity = buzz_identity::Identity::ephemeral();
        let publisher = sign(&folder, "a.b", &identity).unwrap();
        assert_eq!(publisher, identity.pubkey_hex());
        assert_eq!(verify(&folder, "a.b"), Ok(Some(publisher.clone())));
        // Hidden state does not count; the artifact lists the rest.
        std::fs::write(folder.join(".cache/state"), "changed").unwrap();
        assert_eq!(verify(&folder, "a.b"), Ok(Some(publisher)));
        let listed = std::fs::read_to_string(folder.join(ARTIFACT)).unwrap();
        assert!(listed.contains("\"lib/util.py\"") && !listed.contains("state"));
        // Another id, an edited file and an added file all fail.
        assert!(
            verify(&folder, "a.c")
                .unwrap_err()
                .contains("signed as a.b")
        );
        std::fs::write(folder.join("lib/util.py"), "x = 2").unwrap();
        assert!(verify(&folder, "a.b").unwrap_err().contains("changed"));
        std::fs::write(folder.join("lib/util.py"), "x = 1").unwrap();
        std::fs::write(folder.join("extra.py"), "").unwrap();
        assert!(verify(&folder, "a.b").unwrap_err().contains("changed"));
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_tampered_artifact_or_signature_fails() {
        let folder = folder();
        let identity = buzz_identity::Identity::ephemeral();
        sign(&folder, "a.b", &identity).unwrap();
        let mut listed = std::fs::read(folder.join(ARTIFACT)).unwrap();
        listed.push(b' ');
        std::fs::write(folder.join(ARTIFACT), &listed).unwrap();
        assert!(verify(&folder, "a.b").unwrap_err().contains("not the file"));

        sign(&folder, "a.b", &identity).unwrap();
        let signature = std::fs::read_to_string(folder.join(SIGNATURE)).unwrap();
        let forged = signature.replace(FORMAT, "buzz-plugin-folder-v2");
        std::fs::write(folder.join(SIGNATURE), forged).unwrap();
        assert!(verify(&folder, "a.b").unwrap_err().contains("not valid"));

        std::fs::remove_file(folder.join(ARTIFACT)).unwrap();
        std::fs::write(folder.join(SIGNATURE), signature).unwrap();
        assert!(verify(&folder, "a.b").unwrap_err().contains("is not"));
        let _ = std::fs::remove_dir_all(folder);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_signed() {
        let folder = folder();
        std::os::unix::fs::symlink("/etc/hostname", folder.join("link")).unwrap();
        let identity = buzz_identity::Identity::ephemeral();
        assert!(
            sign(&folder, "a.b", &identity)
                .unwrap_err()
                .contains("symlink")
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn publishers_shorten_to_an_npub() {
        let identity = buzz_identity::Identity::ephemeral();
        let short = short_npub(&identity.pubkey_hex());
        assert!(short.starts_with("npub1") && short.contains('…'), "{short}");
    }
}
