//! Manager-owned ChatGPT logins. Agents receive a broker credential, never
//! the upstream refresh token. A consistent journal selects an immutable
//! secret version. Enrollment activates a staged login after writing the
//! profile. The deployment runs one manager with Recreate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, from_str, json, to_string, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error as ThisError;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::timeout;
use url::Url;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use self::journal::Journal;
use crate::enroll::ProfileStore;
use crate::manager::Developer;

mod journal;
mod oauth;
pub mod server;
pub use oauth::OAuth;

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TOKEN_BYTES: usize = 8192;
const EXPIRY_WINDOW: i64 = 300;

/// Refusals contain fixed labels only, including storage and OAuth errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ThisError)]
pub enum Error {
    #[error("invalid ChatGPT login")]
    InvalidLogin,
    #[error("ChatGPT login is not configured")]
    NotConfigured,
    #[error("ChatGPT login is disconnected or replaced")]
    Unauthorized,
    #[error("ChatGPT login storage is unavailable")]
    Storage,
    #[error("a refresh was interrupted; sign in to ChatGPT again")]
    Interrupted,
    #[error("ChatGPT login was rejected; sign in again")]
    Rejected,
    #[error("ChatGPT authentication service is unavailable")]
    Upstream,
    #[error("ChatGPT login is busy; retry")]
    Busy,
}

/// Secret-bearing types deliberately do not implement Debug.
#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
}

/// An owner may connect a dedicated login or disconnect remote agents.
/// The original cache is never written into the agent-readable profile.
pub enum Change {
    Connect(Tokens),
    Disconnect,
}

impl Change {
    /// Parse the optional encrypted enrollment change without exposing its contents.
    pub fn parse(value: Option<&Value>) -> Result<Option<Self>, Error> {
        match value {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(Self::Disconnect)),
            Some(value) => Tokens::from_login(value).map(Self::Connect).map(Some),
        }
    }
}

impl Drop for Tokens {
    fn drop(&mut self) {
        self.id_token.zeroize();
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

fn token(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_TOKEN_BYTES && !value.chars().any(char::is_whitespace)
}

fn claims(bearer: &str) -> Result<Value, Error> {
    if !token(bearer) {
        return Err(Error::InvalidLogin);
    }
    let mut parts = bearer.split('.');
    let (_, Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::InvalidLogin);
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Error::InvalidLogin)?;
    from_slice(&bytes).map_err(|_| Error::InvalidLogin)
}

impl Tokens {
    /// Read a dedicated `codex login` cache, without accepting API-key auth.
    pub fn from_login(value: &Value) -> Result<Self, Error> {
        if value["auth_mode"] != "chatgpt" || !value["OPENAI_API_KEY"].is_null() {
            return Err(Error::InvalidLogin);
        }
        let string = |field: &str| {
            value["tokens"][field]
                .as_str()
                .filter(|v| token(v))
                .map(str::to_owned)
                .ok_or(Error::InvalidLogin)
        };
        let tokens = Self {
            id_token: string("id_token")?,
            access_token: string("access_token")?,
            refresh_token: string("refresh_token")?,
            account_id: string("account_id")?,
        };
        tokens.validate()?;
        Ok(tokens)
    }

    pub(super) fn validate(&self) -> Result<(), Error> {
        if !token(&self.refresh_token) || !token(&self.account_id) {
            return Err(Error::InvalidLogin);
        }
        let identity = claims(&self.id_token)?;
        if identity["https://api.openai.com/auth"]["chatgpt_account_id"].as_str()
            != Some(self.account_id.as_str())
            || claims(&self.access_token)?["exp"].as_i64().is_none()
        {
            return Err(Error::InvalidLogin);
        }
        Ok(())
    }

    fn expiring(&self, now: i64) -> Result<bool, Error> {
        claims(&self.access_token)?["exp"]
            .as_i64()
            .map(|expires| expires <= now.saturating_add(EXPIRY_WINDOW))
            .ok_or(Error::InvalidLogin)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Ready,
    Refreshing,
}

#[derive(Clone, Serialize, Deserialize)]
struct Login {
    credential: String,
    token_version: String,
    state: State,
    tokens: Tokens,
}

impl Drop for Login {
    fn drop(&mut self) {
        self.credential.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    #[serde(skip)]
    revision: Option<String>,
    version: u32,
    active: Option<Login>,
    pending: Option<Login>,
}

/// The sole owner of refreshes. Enrollment takes the same per-developer
/// lock as refresh, including the bounded storage and upstream operations.
pub struct Broker {
    store: Arc<dyn ProfileStore>,
    journal: Arc<dyn Journal>,
    oauth: OAuth,
    developers: BTreeMap<String, (Developer, Arc<Mutex<()>>)>,
    url: String,
    ca_certificate: String,
}

impl Broker {
    fn new(
        store: Arc<dyn ProfileStore>,
        journal: Arc<dyn Journal>,
        oauth: OAuth,
        developers: impl IntoIterator<Item = Developer>,
        url: String,
        ca_certificate: String,
    ) -> Result<Self, Error> {
        let parsed = Url::parse(&url).map_err(|_| Error::NotConfigured)?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !ca_certificate.starts_with("-----BEGIN CERTIFICATE-----\n")
        {
            return Err(Error::NotConfigured);
        }
        Ok(Self {
            store,
            journal,
            oauth,
            developers: developers
                .into_iter()
                .map(|d| (d.id.clone(), (d, Arc::new(Mutex::new(())))))
                .collect(),
            url,
            ca_certificate,
        })
    }

    /// Serialize enrollment and refresh for one configured developer.
    pub async fn lock(&self, developer: &Developer) -> Result<OwnedMutexGuard<()>, Error> {
        let (_, lock) = self
            .developers
            .get(&developer.id)
            .ok_or(Error::NotConfigured)?;
        timeout(LOCK_TIMEOUT, Arc::clone(lock).lock_owned())
            .await
            .map_err(|_| Error::Busy)
    }

    fn secret(developer: &Developer) -> String {
        format!("voltage/staging/agents/{}/codex-login", developer.id)
    }

    async fn read(&self, developer: &Developer) -> Result<Record, Error> {
        let Some(revision) = self.journal.head(developer).await? else {
            return Ok(Record {
                revision: None,
                version: 1,
                active: None,
                pending: None,
            });
        };
        let stored = self
            .store
            .get_version(&Self::secret(developer), &developer.region, &revision)
            .await
            .map_err(|_| Error::Storage)?
            .ok_or(Error::Storage)?;
        if stored.version_id != revision {
            return Err(Error::Storage);
        }
        let mut record: Record = from_str(&stored.text).map_err(|_| Error::Storage)?;
        if record.version != 1 {
            return Err(Error::Storage);
        }
        record.revision = Some(revision);
        Ok(record)
    }

    async fn write(&self, developer: &Developer, record: &mut Record) -> Result<(), Error> {
        let text = Zeroizing::new(to_string(record).map_err(|_| Error::Storage)?);
        let revision = self
            .store
            .put(&Self::secret(developer), &developer.region, &text)
            .await
            .map_err(|_| Error::Storage)?;
        // Secret versions are immutable. Publish their exact IDs with a
        // conditional write before any external effect; never read AWSCURRENT.
        if let Err(error) = self
            .journal
            .advance(developer, record.revision.as_deref(), &revision)
            .await
        {
            // A retry of an applied update fails its own condition; a lost
            // success response must not leave a durable marker unowned.
            if self.journal.head(developer).await?.as_deref() != Some(revision.as_str()) {
                return Err(error);
            }
        }
        record.revision = Some(revision);
        Ok(())
    }

    /// Stage a login without revoking the active login. Caller holds the lock.
    pub async fn stage(&self, developer: &Developer, tokens: Tokens) -> Result<Value, Error> {
        tokens.validate()?;
        let mut record = self.read(developer).await?;
        let credential = format!("{}.{}", developer.id, Uuid::new_v4().simple());
        record.pending = Some(Login {
            credential: credential.clone(),
            token_version: Uuid::new_v4().to_string(),
            state: State::Ready,
            tokens,
        });
        self.write(developer, &mut record).await?;
        Ok(json!({
            "url": self.url,
            "ca_certificate": self.ca_certificate,
            "refresh_token": format!("{credential}.seed"),
        }))
    }

    /// Activate only after committing the profile. Caller holds the lock.
    pub async fn activate(&self, developer: &Developer, credential: &str) -> Result<(), Error> {
        let mut record = self.read(developer).await?;
        if select(&mut record, credential)? {
            self.write(developer, &mut record).await?;
        }
        Ok(())
    }

    /// Revoke even credentials served by a stale profile replica.
    /// Caller holds the lock. Already-issued access tokens expire upstream.
    pub async fn disconnect(&self, developer: &Developer) -> Result<(), Error> {
        let mut record = self.read(developer).await?;
        record.active = None;
        record.pending = None;
        self.write(developer, &mut record).await
    }

    /// Coalesce concurrent stale-token requests into one upstream refresh.
    /// The client persists a broker token carrying the returned version;
    /// an older version receives the already-refreshed bundle.
    pub async fn refresh(&self, credential: &str, now: i64) -> Result<Value, Error> {
        if credential.len() > 256 {
            return Err(Error::Unauthorized);
        }
        let mut parts = credential.split('.');
        let (Some(id), Some(_), Some(requested_version), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::Unauthorized);
        };
        let (developer, _) = self.developers.get(id).ok_or(Error::Unauthorized)?;
        let _lock = self.lock(developer).await?;
        let mut record = self.read(developer).await?;
        let login = record.active.as_mut().ok_or(Error::Unauthorized)?;
        if !same_credential(&format!("{}.seed", login.credential), credential) {
            return Err(Error::Unauthorized);
        }
        if login.state != State::Ready {
            return Err(Error::Interrupted);
        }
        if requested_version == login.token_version || login.tokens.expiring(now)? {
            // Persist intent before contacting the rotating-token issuer.
            // Cancellation or a lost response leaves this marker durable:
            // the next process refuses to reuse an uncertain refresh token.
            login.state = State::Refreshing;
            self.write(developer, &mut record).await?;
            let active = record.active.as_ref().ok_or(Error::NotConfigured)?;
            let tokens = self.oauth.refresh(&active.tokens).await?;
            let active = record.active.as_mut().ok_or(Error::NotConfigured)?;
            active.tokens = tokens;
            active.token_version = Uuid::new_v4().to_string();
            active.state = State::Ready;
            self.write(developer, &mut record).await?;
        }
        let active = record.active.as_ref().ok_or(Error::NotConfigured)?;
        Ok(json!({
            "access_token": active.tokens.access_token,
            "id_token": active.tokens.id_token,
            "refresh_token": format!("{}.{}", active.credential, active.token_version),
            "account_id": active.tokens.account_id,
        }))
    }
}

// Compare fixed-sized digests so mismatches do not expose a token prefix.
fn same_credential(left: &str, right: &str) -> bool {
    fn base(value: &str) -> Option<&str> {
        value.rsplit_once('.').map(|(base, _)| base)
    }
    match (base(left), base(right)) {
        (Some(left), Some(right)) => bool::from(Sha256::digest(left).ct_eq(&Sha256::digest(right))),
        _ => false,
    }
}

fn select(record: &mut Record, credential: &str) -> Result<bool, Error> {
    if record
        .pending
        .as_ref()
        .is_some_and(|login| same_credential(&format!("{}.seed", login.credential), credential))
    {
        record.active = record.pending.take();
        return Ok(true);
    }
    if record
        .active
        .as_ref()
        .is_some_and(|login| same_credential(&format!("{}.seed", login.credential), credential))
    {
        return Ok(false);
    }
    Err(Error::Unauthorized)
}

#[cfg(test)]
#[path = "codex_login_tests.rs"]
mod tests;
