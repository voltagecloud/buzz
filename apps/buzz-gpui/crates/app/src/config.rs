//! `$XDG_CONFIG_HOME/buzz-gpui/config.toml`.
//!
//! ```toml
//! [embedded]
//! credentials = "keyring"   # or "files" on machines without a keyring
//!
//! [[host]]
//! name = "devserver"
//! address = "tcp://devserver.tailnet.ts.net:7420"
//! # The token is read from the OS keyring; `token = "..."` also works.
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Keyring service holding remote host tokens, one account per address.
pub const HOST_TOKEN_SERVICE: &str = "dev.local.buzz.gpui.host-tokens";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub embedded: Embedded,
    #[serde(rename = "host")]
    pub hosts: Vec<RemoteHost>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Embedded {
    pub credentials: CredentialsBackend,
    /// The v1 agents' working directory, gone with them; still read so
    /// older config files load.
    #[allow(dead_code)]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialsBackend {
    #[default]
    Keyring,
    Files,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteHost {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub token: Option<String>,
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("buzz-gpui/config.toml"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|error| {
                tracing::error!(path = %path.display(), %error, "invalid config; using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

/// Rewrites the `[[host]]` tables of the config file, leaving every other
/// setting, and the comments around them, as the user wrote them.
pub fn save_hosts(hosts: &[RemoteHost]) -> Result<(), String> {
    let path = Config::path().ok_or("No config directory")?;
    save_hosts_to(&path, hosts)
}

fn save_hosts_to(path: &Path, hosts: &[RemoteHost]) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("Could not read {}: {error}", path.display())),
    };
    let text = with_hosts(&text, hosts)?;
    let dir = path.parent().ok_or("Invalid config path")?;
    std::fs::create_dir_all(dir).map_err(|error| format!("Could not save settings: {error}"))?;
    // Write a sibling and rename it, so a crash never leaves half a file.
    let partial = path.with_extension("toml.partial");
    // The file can hold host tokens, so only the owner may read it.
    buzz_keychain::write_private(&partial, text.as_bytes())
        .map_err(|error| format!("Could not save settings: {error}"))?;
    std::fs::rename(&partial, path).map_err(|error| format!("Could not save settings: {error}"))
}

fn with_hosts(text: &str, hosts: &[RemoteHost]) -> Result<String, String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| format!("The config file is not valid TOML: {error}"))?;
    if hosts.is_empty() {
        document.remove("host");
        return Ok(document.to_string());
    }
    let mut tables = toml_edit::ArrayOfTables::new();
    for host in hosts {
        let mut table = toml_edit::Table::new();
        table.insert("name", toml_edit::value(&host.name));
        table.insert("address", toml_edit::value(&host.address));
        if let Some(token) = &host.token {
            table.insert("token", toml_edit::value(token));
        }
        tables.push(table);
    }
    document.insert("host", toml_edit::Item::ArrayOfTables(tables));
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXISTING: &str = "\
# My settings
[embedded]
credentials = \"files\" # no keyring here
workspace = \"/srv/agents\"

[[host]]
name = \"old\"
address = \"tcp://old:7420\"
";

    fn host(name: &str, address: &str, token: Option<&str>) -> RemoteHost {
        RemoteHost {
            name: name.into(),
            address: address.into(),
            token: token.map(Into::into),
        }
    }

    #[test]
    fn hosts_round_trip() {
        let hosts = vec![
            host("devserver", "tcp://devserver:7420", None),
            host("lab", "unix:///run/buzzd.sock", Some("secret")),
        ];
        let text = with_hosts("", &hosts).unwrap();
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.hosts, hosts);
    }

    #[test]
    fn other_settings_and_comments_survive() {
        let text = with_hosts(EXISTING, &[host("new", "tcp://new:1", None)]).unwrap();
        assert!(text.contains("# My settings"));
        assert!(text.contains("# no keyring here"));
        let config: Config = toml::from_str(&text).unwrap();
        assert!(matches!(
            config.embedded.credentials,
            CredentialsBackend::Files
        ));
        assert_eq!(
            config.embedded.workspace,
            Some(PathBuf::from("/srv/agents"))
        );
        assert_eq!(config.hosts, vec![host("new", "tcp://new:1", None)]);
    }

    #[test]
    fn removing_the_last_host_drops_the_tables() {
        let text = with_hosts(EXISTING, &[]).unwrap();
        assert!(!text.contains("[[host]]"));
        let config: Config = toml::from_str(&text).unwrap();
        assert!(config.hosts.is_empty());
        assert!(matches!(
            config.embedded.credentials,
            CredentialsBackend::Files
        ));
    }

    #[test]
    fn tokens_are_written_only_when_kept_in_the_file() {
        let text = with_hosts("", &[host("a", "tcp://a:1", None)]).unwrap();
        assert!(!text.contains("token"));
        let text = with_hosts("", &[host("a", "tcp://a:1", Some("t"))]).unwrap();
        assert!(text.contains("token = \"t\""));
    }

    #[test]
    fn invalid_files_are_not_overwritten() {
        assert!(with_hosts("[embedded", &[]).is_err());
    }

    #[test]
    fn saves_through_the_file() {
        let dir = std::env::temp_dir().join(format!("buzz-gpui-config-{}", std::process::id()));
        let path = dir.join("buzz-gpui/config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let hosts = vec![host("lab", "tcp://lab:7420", Some("secret"))];
        save_hosts_to(&path, &hosts).unwrap();
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.hosts, hosts);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        save_hosts_to(&path, &[]).unwrap();
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(config.hosts.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
