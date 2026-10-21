//! What a GitHub release says: its version, which of its assets this
//! platform installs, and the checksum `SHA256SUMS` lists for it.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// The asset the release workflow writes with every package's SHA-256.
pub const CHECKSUMS: &str = "SHA256SUMS";

/// `GET /repos/{owner}/{repo}/releases/latest`, the fields used.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

impl Release {
    /// The tag as a version: `v1.2.3` or `1.2.3`.
    pub fn version(&self) -> Option<semver::Version> {
        let tag = self.tag_name.trim();
        semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
    }

    /// A published, final release newer than `current`. Drafts,
    /// prereleases and suffixed versions never offer themselves.
    pub fn newer_than(&self, current: &semver::Version) -> bool {
        if self.draft || self.prerelease {
            return false;
        }
        self.version()
            .is_some_and(|version| version.pre.is_empty() && version > *current)
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// The package for `platform`, by the names the packaging scripts
    /// give them.
    pub fn asset_for(&self, platform: Platform) -> Option<&Asset> {
        self.assets
            .iter()
            .find(|asset| platform.matches(&asset.name))
    }
}

/// Where this copy runs, as the release names its packages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// `buzz-gpui_<version>-<revision>_<debian arch>.deb`.
    Deb { arch: &'static str },
    /// `Buzz-<version>-macos-<arm64|x86_64>.dmg`.
    MacOs { arch: &'static str },
    /// `Buzz-<version>-windows-<arch>.zip`.
    Windows { arch: &'static str },
    /// No package is published for it.
    Other,
}

impl Platform {
    /// The running platform.
    pub fn current() -> Self {
        Self::of(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn of(os: &str, arch: &str) -> Self {
        match (os, arch) {
            ("linux", "x86_64") => Self::Deb { arch: "amd64" },
            ("linux", "aarch64") => Self::Deb { arch: "arm64" },
            ("macos", "aarch64") => Self::MacOs { arch: "arm64" },
            ("macos", "x86_64") => Self::MacOs { arch: "x86_64" },
            ("windows", "x86_64") => Self::Windows { arch: "x86_64" },
            ("windows", "aarch64") => Self::Windows { arch: "arm64" },
            _ => Self::Other,
        }
    }

    fn matches(self, name: &str) -> bool {
        match self {
            Self::Deb { arch } => {
                name.starts_with("buzz-gpui_") && name.ends_with(&format!("_{arch}.deb"))
            }
            Self::MacOs { arch } => {
                name.starts_with("Buzz-") && name.ends_with(&format!("-macos-{arch}.dmg"))
            }
            Self::Windows { arch } => {
                name.starts_with("Buzz-") && name.ends_with(&format!("-windows-{arch}.zip"))
            }
            Self::Other => false,
        }
    }
}

/// The digest `SHA256SUMS` lists for `file`, in `sha256sum` output
/// (`<hex>  <name>`, or `<hex> *<name>` in binary mode).
pub fn checksum_for(sums: &str, file: &str) -> Option<[u8; 32]> {
    sums.lines().find_map(|line| {
        let (hex, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == file).then(|| parse_hex(hex)).flatten()
    })
}

fn parse_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The SHA-256 of the file at `path`.
pub fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

/// Checks the file at `path` against `expected`.
pub fn verify(path: &Path, expected: &[u8; 32]) -> Result<(), String> {
    let actual =
        sha256_file(path).map_err(|error| format!("Could not read the update: {error}"))?;
    if &actual == expected {
        Ok(())
    } else {
        Err(format!(
            "The download doesn't match its checksum (expected {}, got {})",
            hex(expected),
            hex(&actual)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    const FIXTURE: &str = include_str!("fixtures/release.json");

    fn release() -> Release {
        serde_json::from_str(FIXTURE).unwrap()
    }

    fn v(text: &str) -> semver::Version {
        semver::Version::parse(text).unwrap()
    }

    #[test]
    fn parses_the_api_response() {
        let release = release();
        assert_eq!(release.tag_name, "v0.1.1");
        assert_eq!(release.version(), Some(v("0.1.1")));
        assert!(!release.draft && !release.prerelease);
        assert!(release.body.as_deref().unwrap().contains("What's Changed"));
        assert_eq!(release.assets.len(), 6);
        assert_eq!(
            release.asset(CHECKSUMS).unwrap().browser_download_url,
            "https://github.com/benthecarman/buzz-gpui/releases/download/v0.1.1/SHA256SUMS"
        );
    }

    #[test]
    fn compares_versions() {
        let release = release();
        assert!(release.newer_than(&v("0.1.0")));
        assert!(!release.newer_than(&v("0.1.1")));
        assert!(!release.newer_than(&v("0.2.0")));
        // A prerelease of the running version is older than the release.
        assert!(release.newer_than(&v("0.1.1-rc.1")));
        let numeric = Release {
            tag_name: "0.10.0".into(),
            ..release.clone()
        };
        assert!(numeric.newer_than(&v("0.9.9")));
    }

    #[test]
    fn skips_drafts_prereleases_and_odd_tags() {
        let base = release();
        let current = v("0.1.0");
        let draft = Release {
            draft: true,
            ..base.clone()
        };
        let pre = Release {
            prerelease: true,
            ..base.clone()
        };
        let suffixed = Release {
            tag_name: "v0.2.0-rc.1".into(),
            ..base.clone()
        };
        let junk = Release {
            tag_name: "nightly".into(),
            ..base
        };
        for release in [draft, pre, suffixed, junk] {
            assert!(!release.newer_than(&current), "{}", release.tag_name);
        }
    }

    #[test]
    fn picks_the_asset_for_each_platform() {
        let release = release();
        let name = |os, arch| {
            release
                .asset_for(Platform::of(os, arch))
                .map(|asset| asset.name.as_str())
        };
        assert_eq!(name("linux", "x86_64"), Some("buzz-gpui_0.1.1-1_amd64.deb"));
        assert_eq!(name("linux", "aarch64"), None);
        assert_eq!(name("macos", "aarch64"), Some("Buzz-0.1.1-macos-arm64.dmg"));
        assert_eq!(name("macos", "x86_64"), Some("Buzz-0.1.1-macos-x86_64.dmg"));
        assert_eq!(
            name("windows", "x86_64"),
            Some("Buzz-0.1.1-windows-x86_64.zip")
        );
        assert_eq!(name("freebsd", "x86_64"), None);
    }

    #[test]
    fn reads_sha256sum_output() {
        let sums = "\
0000000000000000000000000000000000000000000000000000000000000001  Buzz-0.1.1-macos-arm64.dmg
ABCDEF00000000000000000000000000000000000000000000000000000000ff *buzz-gpui_0.1.1-1_amd64.deb
nothex  broken.zip
";
        let deb = checksum_for(sums, "buzz-gpui_0.1.1-1_amd64.deb").unwrap();
        assert_eq!(deb[0], 0xab);
        assert_eq!(deb[31], 0xff);
        assert_eq!(
            checksum_for(sums, "Buzz-0.1.1-macos-arm64.dmg").unwrap()[31],
            1
        );
        assert_eq!(checksum_for(sums, "broken.zip"), None);
        assert_eq!(checksum_for(sums, "missing.deb"), None);
        // A name must match whole, not as a suffix.
        assert_eq!(checksum_for(sums, "arm64.dmg"), None);
    }

    #[test]
    fn verifies_files() {
        let dir = std::env::temp_dir().join(format!("buzz-update-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("package.deb");
        std::fs::write(&path, b"hello\n").unwrap();
        // `printf 'hello\n' | sha256sum`
        let sums = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03  package.deb";
        let expected = checksum_for(sums, "package.deb").unwrap();
        assert_eq!(verify(&path, &expected), Ok(()));
        std::fs::write(&path, b"tampered\n").unwrap();
        assert!(verify(&path, &expected).unwrap_err().contains("checksum"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
