//! Where a host keeps its agents' private keys.

use buzz_agent_controller::{Credentials, LegacySource, Secret};
use buzz_keychain::Error as StoreError;
use std::io::Write;
use std::path::PathBuf;

/// Keys in the OS credential store (Secret Service on Linux). Used where a
/// desktop session runs one, such as a laptop.
pub struct KeyringCredentials {
    service: String,
}

impl KeyringCredentials {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

impl Credentials for KeyringCredentials {
    /// Old Buzz keeps every agent key in one JSON map, `{"agent:<pubkey>":
    /// nsec}`, under the account `secrets` of its own service.
    fn read_legacy(&self, source: LegacySource, pubkey: &str) -> Result<Secret, String> {
        let bytes =
            buzz_keychain::read(source.keyring_service(), "secrets").map_err(store_message)?;
        let mut map: std::collections::BTreeMap<String, String> =
            serde_json::from_slice(&bytes).map_err(|_| "Saved agent key is corrupt")?;
        let key = map
            .get(&format!("agent:{pubkey}"))
            .ok_or_else(|| "The old Buzz install has no key for this agent".to_string())
            .and_then(|value| Secret::parse(value, pubkey));
        for value in map.values_mut() {
            zeroize::Zeroize::zeroize(value);
        }
        key
    }

    fn read(&self, id: &str, pubkey: &str) -> Result<Option<Secret>, String> {
        let account = account(id, pubkey)?;
        match buzz_keychain::read(&self.service, &account) {
            Ok(bytes) => parse(&bytes, pubkey).map(Some),
            Err(StoreError::Absent) => Ok(None),
            Err(error) => Err(store_message(error)),
        }
    }

    fn add(&self, id: &str, key: &Secret) -> Result<(), String> {
        let account = account(id, key.pubkey())?;
        buzz_keychain::add(&self.service, &account, key.hex().as_bytes()).map_err(store_message)
    }

    fn delete(&self, id: &str, pubkey: &str) -> Result<(), String> {
        let account = account(id, pubkey)?;
        match buzz_keychain::delete(&self.service, &account) {
            Ok(()) | Err(StoreError::Absent) => Ok(()),
            Err(error) => Err(store_message(error)),
        }
    }
}

/// One owner-only file per key. Used on headless machines with no credential
/// store, where the directory's permissions are the protection.
pub struct FileCredentials {
    directory: PathBuf,
}

impl FileCredentials {
    pub fn new(directory: PathBuf) -> std::io::Result<Self> {
        buzz_keychain::private_dir(&directory)?;
        Ok(Self { directory })
    }

    fn path(&self, id: &str, pubkey: &str) -> Result<PathBuf, String> {
        account(id, pubkey)?;
        Ok(self.directory.join(format!("{id}.key")))
    }
}

impl Credentials for FileCredentials {
    fn read_legacy(&self, _: LegacySource, _: &str) -> Result<Secret, String> {
        Err(LEGACY_UNSUPPORTED.into())
    }

    fn read(&self, id: &str, pubkey: &str) -> Result<Option<Secret>, String> {
        let path = self.path(id, pubkey)?;
        match std::fs::read(&path) {
            Ok(bytes) => parse(&bytes, pubkey).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("Could not read the agent key file".into()),
        }
    }

    /// Create-only, like the OS stores: an existing key is never replaced.
    fn add(&self, id: &str, key: &Secret) -> Result<(), String> {
        let path = self.path(id, key.pubkey())?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => "An agent key is already saved".to_string(),
            _ => "Could not save the agent key file".to_string(),
        })?;
        file.write_all(key.hex().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|_| {
                let _ = std::fs::remove_file(&path);
                "Could not save the agent key file".to_string()
            })
    }

    fn delete(&self, id: &str, pubkey: &str) -> Result<(), String> {
        let path = self.path(id, pubkey)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Could not delete the agent key file".into()),
        }
    }
}

fn store_message(error: StoreError) -> String {
    match error {
        StoreError::Absent => "No agent key is saved",
        StoreError::Occupied => "An agent key is already saved",
        StoreError::Denied => "Access to the credential store was denied",
        StoreError::Corrupt => "Saved agent key is corrupt",
        StoreError::Unavailable => "The credential store is unavailable",
        StoreError::Busy => "The credential store is busy; try again",
    }
    .into()
}

const LEGACY_UNSUPPORTED: &str = "Importing keys from old Buzz is not supported on this host";

/// Credential IDs are `<agent pubkey>-<community digest>`, both 64 hex chars.
fn account(id: &str, pubkey: &str) -> Result<String, String> {
    let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match id.split_once('-') {
        Some((key, community)) if hex(key) && hex(community) && key == pubkey => {
            Ok(format!("agent:{id}"))
        }
        _ => Err("Invalid agent credential identifier".into()),
    }
}

fn parse(bytes: &[u8], pubkey: &str) -> Result<Secret, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Saved agent key is corrupt")?;
    Secret::parse(text.trim(), pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(key: &Secret) -> String {
        format!("{}-{}", key.pubkey(), "ab".repeat(32))
    }

    #[test]
    fn file_keys_round_trip_and_are_create_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentials::new(dir.path().join("keys")).unwrap();
        let key = Secret::generate().unwrap();
        let id = id(&key);

        assert!(store.read(&id, key.pubkey()).unwrap().is_none());
        store.add(&id, &key).unwrap();
        let read = store.read(&id, key.pubkey()).unwrap().unwrap();
        assert_eq!(read.hex(), key.hex());
        assert!(store.add(&id, &Secret::generate().unwrap()).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.path().join("keys")), 0o700);
            assert_eq!(
                mode(&dir.path().join("keys").join(format!("{id}.key"))),
                0o600
            );
        }

        store.delete(&id, key.pubkey()).unwrap();
        store.delete(&id, key.pubkey()).unwrap();
        assert!(store.read(&id, key.pubkey()).unwrap().is_none());
    }

    #[test]
    fn rejects_identifiers_that_do_not_match_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentials::new(dir.path().to_path_buf()).unwrap();
        let key = Secret::generate().unwrap();
        let other = Secret::generate().unwrap();
        assert!(store.add(&id(&other), &key).is_err());
        assert!(store.read("../escape", key.pubkey()).is_err());
    }
}
