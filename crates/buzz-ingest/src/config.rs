//! Nonsecret provider-to-channel mappings. Credentials come only from env.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub relay_url: String,
    #[serde(default)]
    pub github_repos: BTreeMap<String, Uuid>,
    #[serde(default)]
    pub linear_teams: BTreeMap<String, Uuid>,
    #[serde(default)]
    pub linear_projects: BTreeMap<String, Uuid>,
    #[serde(default)]
    pub linear_webhooks: BTreeMap<String, Uuid>,
    #[serde(default)]
    pub linear_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub slack_channels: BTreeMap<String, Uuid>,
    #[serde(default)]
    pub slack_workspace_url: String,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        secure_url(&self.relay_url)?;
        let count = self.github_repos.len()
            + self.linear_teams.len()
            + self.linear_projects.len()
            + self.linear_webhooks.len()
            + self.slack_channels.len();
        if count == 0 || count > 256 {
            bail!("configure between 1 and 256 provider mappings");
        }
        if self.linear_labels.len() > 256
            || self.linear_labels.iter().any(|(id, name)| {
                id.is_empty() || id.len() > 256 || name.is_empty() || name.len() > 256
            })
        {
            bail!("configure at most 256 nonempty Linear label names, each at most 256 bytes");
        }
        if !self.slack_channels.is_empty() {
            let url = secure_url(&self.slack_workspace_url)?;
            if url.scheme() != "https"
                || !url
                    .host_str()
                    .is_some_and(|host| host.ends_with(".slack.com"))
            {
                bail!("Slack workspace URL must use https://<workspace>.slack.com");
            }
            if self.slack_channels.keys().any(|key| {
                !key.split_once('/').is_some_and(|(team, channel)| {
                    team.starts_with('T')
                        && channel.starts_with('C')
                        && team
                            .bytes()
                            .chain(channel.bytes())
                            .all(|b| b.is_ascii_alphanumeric())
                })
            }) {
                bail!("Slack mappings require team/public-channel IDs");
            }
        }
        Ok(())
    }
}

pub(crate) fn secure_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).context("invalid endpoint URL")?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("endpoint must use HTTPS without path, credentials, query, or fragment (loopback HTTP is allowed)");
    }
    Ok(url)
}

pub(crate) struct Secrets {
    pub github: Option<String>,
    pub linear: Option<String>,
    pub slack: Option<String>,
}

impl Secrets {
    pub fn from_env(config: &Config) -> Result<Self> {
        let get = |name: &str, required: bool| -> Result<Option<String>> {
            match std::env::var(name) {
                Ok(value) if !value.is_empty() && value.len() <= 8192 => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) if !required => Ok(None),
                _ => bail!("missing or invalid {name}"),
            }
        };
        Ok(Self {
            github: get("BUZZ_INGEST_GITHUB_SECRET", !config.github_repos.is_empty())?,
            linear: get(
                "BUZZ_INGEST_LINEAR_SECRET",
                !(config.linear_teams.is_empty()
                    && config.linear_projects.is_empty()
                    && config.linear_webhooks.is_empty()),
            )?,
            slack: get(
                "BUZZ_INGEST_SLACK_SECRET",
                !config.slack_channels.is_empty(),
            )?,
        })
    }
}
