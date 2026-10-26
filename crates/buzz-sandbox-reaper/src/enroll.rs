//! `enroll` (`docs/remote-agents.md` §Relay transport): store a developer's
//! agent profile in their Secrets Manager secret, on a request signed by
//! that developer's owner key.
//!
//! The checks that need nothing external run at admission
//! ([`admit_profile`]); the GitHub login check and the write run as the
//! request's job ([`Enrollment::run`]). Nothing here logs, and no refusal
//! quotes a profile value.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, SdkConfig};
use aws_sdk_secretsmanager::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_secretsmanager::operation::get_secret_value::GetSecretValueOutput;
use aws_sdk_secretsmanager::{
    config::{Builder, Region},
    primitives::DateTimeFormat,
    Client,
};
use buzz_backend_kubernetes::wire::ErrorCode;
use nostr::PublicKey;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::codex_login::{Broker, Change};
use crate::manager::{refusal, Developer};
use crate::profile;

pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
const GITHUB_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest `GET /user` body read; GitHub's is a few KB.
const MAX_GITHUB_BODY: usize = 256 * 1024;

/// Enrollment steps 2 to 4, in order: the profile names the signing owner
/// and that owner's developer, and has a partial profile's shape
/// ([`profile::check_partial`]). The merged profile is validated when the
/// job runs. A profile without a developer ID gets the signing owner's, so a
/// developer never needs to know the Terraform key they are listed under.
/// Returns the profile to merge, or the refusal.
pub fn admit_profile(
    payload: &Value,
    owner: &PublicKey,
    developer: &Developer,
) -> Result<Value, (ErrorCode, &'static str)> {
    let mut profile = payload
        .as_object()
        .filter(|payload| {
            payload.contains_key("profile")
                && payload
                    .keys()
                    .all(|key| key == "profile" || key == "codex_login")
        })
        .and_then(|payload| payload.get("profile"))
        .filter(|profile| profile.is_object())
        .cloned()
        .ok_or((
            ErrorCode::InvalidProfile,
            "the enroll payload must be {\"profile\": <object>}",
        ))?;
    let named = profile.get("developer").and_then(Value::as_str);
    if named.is_none_or(str::is_empty) {
        profile["developer"] = Value::String(developer.id.clone());
    }
    if profile.get("owner_pubkey").and_then(Value::as_str) != Some(owner.to_hex().as_str()) {
        return Err((
            ErrorCode::ProfileOwnerMismatch,
            "the profile's owner_pubkey is not the key that signed the request",
        ));
    }
    if profile.get("developer").and_then(Value::as_str) != Some(developer.id.as_str()) {
        return Err((
            ErrorCode::ProfileDeveloperMismatch,
            "the profile's developer is not the signing owner's developer ID",
        ));
    }
    profile::check_partial(&profile)
        .map_err(|profile::Invalid(message)| (ErrorCode::InvalidProfile, message))?;
    Ok(profile)
}

/// Why a profile write failed: a stable label, safe to log and to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreFailed(pub &'static str);

/// The current version of a developer's profile secret.
pub struct Stored {
    pub text: Zeroizing<String>,
    pub version_id: String,
    /// When the version was written, as RFC 3339.
    pub created: Option<String>,
}

/// Where profiles are read and written. A trait because Secrets Manager is a
/// real external boundary: production uses [`SecretsManagerStore`], tests an
/// in-memory store.
pub trait ProfileStore: Send + Sync {
    /// The current version of the secret `arn` in `region`, or `None` when
    /// it has no value yet.
    fn get<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Stored>, StoreFailed>> + Send + 'a>>;

    /// Read an immutable version. Failure must never fall back to AWSCURRENT.
    fn get_version<'a>(
        &'a self,
        _arn: &'a str,
        _region: &'a str,
        _version: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Stored>, StoreFailed>> + Send + 'a>> {
        Box::pin(async { Err(StoreFailed("version_reads_not_supported")) })
    }

    /// Write `profile` as the new current version of the secret `arn` in
    /// `region`, and return the version id.
    fn put<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
        profile: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, StoreFailed>> + Send + 'a>>;
}

/// Secrets Manager `GetSecretValue` and `PutSecretValue` with the ambient
/// credentials (IRSA in-cluster). It writes no files.
pub struct SecretsManagerStore {
    shared: SdkConfig,
}

impl SecretsManagerStore {
    pub async fn new() -> Self {
        let timeouts = TimeoutConfig::builder()
            .connect_timeout(Duration::from_secs(5))
            .operation_attempt_timeout(Duration::from_secs(10))
            .operation_timeout(Duration::from_secs(25))
            .build();
        let shared = aws_config::defaults(BehaviorVersion::latest())
            .timeout_config(timeouts)
            .load()
            .await;
        Self { shared }
    }
}

impl SecretsManagerStore {
    fn client(&self, region: &str) -> Client {
        let config = Builder::from(&self.shared)
            .region(Region::new(region.to_string()))
            .build();
        Client::from_conf(config)
    }
}

impl ProfileStore for SecretsManagerStore {
    fn get<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Stored>, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            let output = match self
                .client(region)
                .get_secret_value()
                .secret_id(arn)
                .send()
                .await
            {
                Ok(output) => output,
                // The secret exists but holds no version until the first enroll.
                Err(SdkError::ServiceError(service))
                    if service.err().code() == Some("ResourceNotFoundException") =>
                {
                    return Ok(None)
                }
                Err(error) => return Err(store_error(error)),
            };
            Ok(stored_value(output))
        })
    }

    fn get_version<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
        version: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Stored>, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            let output = self
                .client(region)
                .get_secret_value()
                .secret_id(arn)
                .version_id(version)
                .send()
                .await
                .map_err(store_error)?;
            Ok(stored_value(output))
        })
    }

    fn put<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
        profile: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            let output = self
                .client(region)
                .put_secret_value()
                .secret_id(arn)
                .secret_string(profile)
                .send()
                .await
                .map_err(store_error)?;
            output
                .version_id()
                .map(str::to_string)
                .ok_or(StoreFailed("no_version_id"))
        })
    }
}

fn stored_value(output: GetSecretValueOutput) -> Option<Stored> {
    let created = output
        .created_date()
        .and_then(|at| at.fmt(DateTimeFormat::DateTime).ok());
    Some(Stored {
        text: Zeroizing::new(output.secret_string()?.to_owned()),
        version_id: output.version_id()?.to_owned(),
        created,
    })
}

/// A label for an SDK error. The error itself is never logged or returned.
fn store_error<E: ProvideErrorMetadata, R>(error: SdkError<E, R>) -> StoreFailed {
    StoreFailed(match &error {
        SdkError::TimeoutError(_) => "timeout",
        SdkError::DispatchFailure(failure) if failure.is_timeout() => "timeout",
        SdkError::ServiceError(service) => match service.err().code() {
            Some("AccessDeniedException") => "access_denied",
            Some("ResourceNotFoundException") => "not_found",
            _ => "service_error",
        },
        _ => "unavailable",
    })
}

/// The GitHub login check: `GET <api>/user` with the profile's token.
pub struct GitHub {
    client: reqwest::Client,
    user_url: String,
}

impl GitHub {
    /// `api_url` is `https://`, or `http://` to a loopback host for tests.
    pub fn new(api_url: &str) -> Result<Self, String> {
        let url = reqwest::Url::parse(api_url).map_err(|_| "not a URL".to_string())?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err("must be an https:// URL".into());
        }
        let client = reqwest::Client::builder()
            .timeout(GITHUB_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("buzz-sandbox-manager")
            .build()
            .map_err(|_| "could not build the HTTP client".to_string())?;
        Ok(Self {
            client,
            user_url: format!("{}/user", api_url.trim_end_matches('/')),
        })
    }

    /// The login GitHub reports for `token`. The refusal says which step
    /// failed and nothing GitHub sent.
    async fn login(&self, token: &str) -> Result<String, &'static str> {
        let mut response = self
            .client
            .get(&self.user_url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|_| "could not reach GitHub to check the profile's token; retry")?;
        if !response.status().is_success() {
            return Err("GitHub refused the profile's token");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "could not read GitHub's reply; retry")?
        {
            if body.len() + chunk.len() > MAX_GITHUB_BODY {
                return Err("GitHub's reply is too large");
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|user| user.get("login")?.as_str().map(str::to_string))
            .ok_or("GitHub did not report a login for the profile's token")
    }
}

/// Enrollment steps 5 and 6.
pub struct Enrollment {
    github: GitHub,
    store: Arc<dyn ProfileStore>,
    broker: Option<Arc<Broker>>,
}

impl Enrollment {
    pub fn new(github: GitHub, store: Arc<dyn ProfileStore>) -> Self {
        Self {
            github,
            store,
            broker: None,
        }
    }

    pub fn with_broker(mut self, broker: Arc<Broker>) -> Self {
        self.broker = Some(broker);
        self
    }

    /// The developer's stored profile, parsed. An unreadable stored value
    /// counts as none, so the next enroll must send a complete profile.
    async fn stored(&self, developer: &Developer) -> Result<Option<(Value, Stored)>, Value> {
        match self
            .store
            .get(&developer.profile_secret_arn, &developer.region)
            .await
        {
            Ok(Some(stored)) => Ok(serde_json::from_str(&stored.text)
                .ok()
                .map(|profile| (profile, stored))),
            Ok(None) => Ok(None),
            Err(StoreFailed(label)) => Err(refusal(
                ErrorCode::ProfileStoreFailed,
                &format!("could not read the stored profile ({label}); retry"),
            )),
        }
    }

    /// Merge `partial` into the stored profile, validate the result, check a
    /// new GitHub token's login, then write it and answer with the new
    /// version id and its [`profile::summary`]. Every profile copy is
    /// dropped, and the serialized one zeroized, before this returns.
    pub async fn run(&self, developer: &Developer, partial: Value) -> Value {
        self.run_with_login(developer, partial, None).await
    }

    /// Enroll or revoke a dedicated login under the same lock as token refresh.
    pub async fn run_with_login(
        &self,
        developer: &Developer,
        partial: Value,
        login: Option<Change>,
    ) -> Value {
        let _guard = match &self.broker {
            Some(broker) => match broker.lock(developer).await {
                Ok(guard) => Some(guard),
                Err(error) => return refusal(ErrorCode::ProfileStoreFailed, &error.to_string()),
            },
            None if matches!(login, Some(Change::Connect(_))) => {
                return refusal(
                    ErrorCode::InvalidProfile,
                    "ChatGPT login is not configured on this manager",
                );
            }
            None => None,
        };
        let stored = match self.stored(developer).await {
            Ok(stored) => stored.map(|(profile, _)| profile),
            Err(refused) => return refused,
        };
        if stored.is_none()
            && ["git", "github", "via"]
                .iter()
                .any(|f| partial.get(f).is_none())
        {
            return refusal(
                ErrorCode::InvalidProfile,
                "no profile is on file yet; send the complete profile",
            );
        }
        let new_token = stored.is_none() || partial.get("github").is_some();
        let connects_codex = matches!(login, Some(Change::Connect(_)));
        let disconnects_codex = matches!(login, Some(Change::Disconnect));
        let replaces_codex = partial
            .pointer("/models/openai_api_key")
            .is_some_and(Value::is_string)
            || partial
                .pointer("/models/codex_access_token")
                .is_some_and(Value::is_string)
            || partial.get("codex_login").is_some_and(Value::is_null);
        if self.broker.is_none()
            && (replaces_codex || disconnects_codex)
            && stored
                .as_ref()
                .is_some_and(|p| p.get("codex_login").is_some())
        {
            return refusal(
                ErrorCode::ProfileStoreFailed,
                "ChatGPT broker is unavailable; enable it before disconnecting this login",
            );
        }
        let mut profile = profile::merge(stored, partial);
        if replaces_codex || login.is_some() {
            if let Some(profile) = profile.as_object_mut() {
                profile.remove("codex_login");
            }
        }
        if matches!(login, Some(Change::Connect(_))) {
            if let Some(models) = profile["models"].as_object_mut() {
                models.remove("openai_api_key");
                models.remove("codex_access_token");
            }
        }
        if let Err(profile::Invalid(message)) = profile::validate(&profile) {
            return refusal(ErrorCode::InvalidProfile, message);
        }
        if new_token {
            // Both are strings: the merged profile passed validation.
            let github = &profile["github"];
            let token = github["token"].as_str().unwrap_or_default();
            let login = github["login"].as_str().unwrap_or_default();
            match self.github.login(token).await {
                Ok(actual) if actual.eq_ignore_ascii_case(login) => {}
                Ok(_) => {
                    return refusal(
                        ErrorCode::GithubLoginMismatch,
                        "the profile's GitHub token belongs to a different login",
                    )
                }
                Err(message) => return refusal(ErrorCode::GithubLoginMismatch, message),
            }
        }
        if let Some(Change::Connect(tokens)) = login {
            let Some(broker) = &self.broker else {
                return refusal(
                    ErrorCode::InvalidProfile,
                    "ChatGPT login is not configured on this manager",
                );
            };
            match broker.stage(developer, tokens).await {
                Ok(metadata) => profile["codex_login"] = metadata,
                Err(error) => return refusal(ErrorCode::ProfileStoreFailed, &error.to_string()),
            }
        }
        if let Err(profile::Invalid(message)) = profile::validate(&profile) {
            return refusal(ErrorCode::InvalidProfile, message);
        }
        // Revoke before changing the profile: stale Secrets Manager replicas
        // may still serve its old broker credential, which must no longer work.
        if !connects_codex && (disconnects_codex || replaces_codex) {
            if let Some(broker) = &self.broker {
                if let Err(error) = broker.disconnect(developer).await {
                    return refusal(ErrorCode::ProfileStoreFailed, &error.to_string());
                }
            }
        }
        let text = Zeroizing::new(profile.to_string());
        match self
            .store
            .put(&developer.profile_secret_arn, &developer.region, &text)
            .await
        {
            Ok(version_id) => {
                if let Some(broker) = &self.broker {
                    if let Some(credential) = profile
                        .pointer("/codex_login/refresh_token")
                        .and_then(Value::as_str)
                    {
                        if let Err(error) = broker.activate(developer, credential).await {
                            return refusal(ErrorCode::ProfileStoreFailed, &error.to_string());
                        }
                    }
                }
                let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let summary = profile::summary(&profile, &version_id, Some(now));
                serde_json::json!({"ok": true, "version_id": version_id, "profile": summary})
            }
            Err(StoreFailed(label)) => refusal(
                ErrorCode::ProfileStoreFailed,
                &format!("could not store the profile ({label}); retry"),
            ),
        }
    }

    /// The `profile` operation: the stored profile's [`profile::summary`],
    /// or `null` when none is stored.
    pub async fn describe(&self, developer: &Developer) -> Value {
        match self.stored(developer).await {
            Ok(Some((profile, stored))) => serde_json::json!({
                "ok": true,
                "profile": profile::summary(&profile, &stored.version_id, stored.created),
            }),
            Ok(None) => serde_json::json!({"ok": true, "profile": null}),
            Err(refused) => refused,
        }
    }
}

#[cfg(test)]
#[path = "enroll_tests.rs"]
pub(crate) mod tests;
