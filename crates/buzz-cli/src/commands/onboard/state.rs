//! What `buzz onboard` keeps between runs, so a partial onboarding resumes
//! where it stopped. Everything lives in one private directory (mode
//! `0700`): the owner key in `identity.nsec` and the progress in
//! `onboard.json`, both mode `0600`, each written atomically. Model, GitHub
//! and 1Password credentials are never written here: they go only to the
//! Sandbox manager, encrypted, as the Buzz app sends them.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

/// The state file's format version.
pub const VERSION: u32 = 1;

/// A secret string: wiped on drop, redacted in `Debug`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps `text`.
    pub fn new(text: String) -> Self {
        Self(Zeroizing::new(text))
    }

    /// The secret text. Never print it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Secret::new)
    }
}

/// The workspace channel the agents were added to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRef {
    /// The channel UUID.
    pub id: String,
    /// Its name when chosen.
    pub name: String,
}

/// One agent `buzz onboard` created, and how far its setup got.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Display name.
    pub name: String,
    /// The preset it was made from.
    pub preset: String,
    /// The harness: `claude`, `codex` or `buzz-agent`.
    pub runtime: String,
    /// Empty uses the harness default.
    pub model: String,
    /// Empty uses the model default.
    pub effort: String,
    /// The harness system prompt.
    pub instructions: String,
    /// The agent's public key, hex.
    pub pubkey: String,
    /// The agent's secret key, `nsec`. The Sandbox manager receives it
    /// encrypted in the first start, as from the Buzz app's host.
    pub secret: Secret,
    /// The owner's NIP-OA attestation JSON for this agent.
    pub auth_tag: String,
    /// The agent-signed kind:0 profile was accepted.
    #[serde(default)]
    pub profile_published: bool,
    /// The owner's kind 30177 record was accepted.
    #[serde(default)]
    pub listed: bool,
    /// The agent is a member of the workspace channel.
    #[serde(default)]
    pub member: bool,
    /// The manager stored its launch registration (first start succeeded).
    #[serde(default)]
    pub registered: bool,
    /// The signed first-start message, saved before any publication or deploy.
    #[serde(default)]
    pub kickoff: Option<nostr::Event>,
    /// The relay acknowledged the kickoff before a deploy could be attempted.
    #[serde(default)]
    pub kickoff_published: bool,
}

/// A signing key the manager stored but GitHub may not have yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingKey {
    /// The GitHub login it belongs on.
    pub login: String,
    /// The key's title on GitHub.
    pub title: String,
    /// The public OpenSSH line (not secret).
    pub public: String,
}

/// A gateway grant the relay accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayGrant {
    /// The granted gateway key, hex.
    pub gateway: String,
    /// When the grant expires, unix seconds.
    pub expires_at: u64,
}

/// Onboarding progress. Holds no model, GitHub or 1Password credential.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Always [`VERSION`].
    pub version: u32,
    /// The community, as a canonical `wss://` origin.
    #[serde(default)]
    pub relay: String,
    /// The owner public key, hex.
    #[serde(default)]
    pub owner_pubkey: String,
    /// The owner is a member of the community.
    #[serde(default)]
    pub joined: bool,
    /// The owner has a kind:0 profile.
    #[serde(default)]
    pub owner_profile: bool,
    /// The workspace channel.
    #[serde(default)]
    pub channel: Option<ChannelRef>,
    /// The agents created here.
    #[serde(default)]
    pub agents: Vec<AgentRecord>,
    /// The last profile version the manager stored.
    #[serde(default)]
    pub enrolled_version: Option<String>,
    /// A stored signing key still to add on GitHub.
    #[serde(default)]
    pub pending_key: Option<PendingKey>,
    /// The Paseo daemon's last accepted gateway grant.
    #[serde(default)]
    pub gateway_grant: Option<GatewayGrant>,
}

/// The private directory and its two files.
pub struct Store {
    dir: PathBuf,
}

const STATE_FILE: &str = "onboard.json";
const IDENTITY_FILE: &str = "identity.nsec";

impl Store {
    /// The store at `dir`, created on first write.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `$XDG_CONFIG_HOME/buzz` (or the platform's config directory).
    pub fn default_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("buzz"))
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the owner key lives.
    pub fn identity_path(&self) -> PathBuf {
        self.dir.join(IDENTITY_FILE)
    }

    /// The saved progress, or a fresh state.
    pub fn load(&self) -> Result<State, String> {
        let path = self.dir.join(STATE_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let state: State = serde_json::from_slice(&bytes)
                    .map_err(|_| format!("{} is not valid onboarding state", path.display()))?;
                if state.version != VERSION {
                    return Err(format!(
                        "{} has unsupported version {}",
                        path.display(),
                        state.version
                    ));
                }
                Ok(state)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State {
                version: VERSION,
                ..State::default()
            }),
            Err(_) => Err(format!("could not read {}", path.display())),
        }
    }

    /// Saves `state` atomically, mode `0600`.
    pub fn save(&self, state: &State) -> Result<(), String> {
        let bytes = Zeroizing::new(
            serde_json::to_vec_pretty(state).map_err(|_| "could not encode onboarding state")?,
        );
        self.write_private(STATE_FILE, &bytes)
    }

    /// The saved owner key, if any.
    pub fn load_identity(&self) -> Result<Option<Secret>, String> {
        let path = self.identity_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(Secret::new(text.trim().to_owned()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(format!("could not read {}", path.display())),
        }
    }

    /// Saves the owner key (`nsec`), mode `0600`.
    pub fn save_identity(&self, nsec: &Secret) -> Result<(), String> {
        let text = Zeroizing::new(format!("{}\n", nsec.expose()));
        self.write_private(IDENTITY_FILE, text.as_bytes())
    }

    /// Writes `name` in the private directory through a `0600` temporary
    /// file in the same directory, then renames it into place.
    fn write_private(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        create_private_dir(&self.dir)?;
        let target = self.dir.join(name);
        let failed = || format!("could not write {}", target.display());
        let mut file = tempfile::Builder::new()
            .prefix(".onboard-")
            .tempfile_in(&self.dir)
            .map_err(|_| failed())?;
        restrict(file.path(), 0o600).map_err(|_| failed())?;
        file.write_all(bytes).map_err(|_| failed())?;
        file.as_file().sync_all().map_err(|_| failed())?;
        file.persist(&target).map_err(|_| failed())?;
        restrict(&target, 0o600).map_err(|_| failed())
    }
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|_| format!("could not create {}", dir.display()))?;
    restrict(dir, 0o700).map_err(|_| format!("could not restrict {}", dir.display()))
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> std::io::Result<()> {
    // Windows ACLs inherit the user profile's private configuration directory.
    Ok(())
}
