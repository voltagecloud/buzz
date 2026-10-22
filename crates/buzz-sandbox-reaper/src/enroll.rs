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

use aws_sdk_secretsmanager::error::{ProvideErrorMetadata, SdkError};
use buzz_backend_kubernetes::wire::ErrorCode;
use nostr::PublicKey;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::manager::{refusal, Developer};
use crate::profile;

pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
const GITHUB_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest `GET /user` body read; GitHub's is a few KB.
const MAX_GITHUB_BODY: usize = 256 * 1024;

/// Enrollment steps 2 to 4, in order: the profile names the signing owner
/// and that owner's developer, and passes `profile.py`'s validation.
/// A profile without a developer ID gets the signing owner's, so a
/// developer never needs to know the Terraform key they are listed under.
/// Returns the profile to store, or the refusal.
pub fn admit_profile(
    payload: &Value,
    owner: &PublicKey,
    developer: &Developer,
) -> Result<Value, (ErrorCode, &'static str)> {
    let mut profile = payload
        .as_object()
        .filter(|payload| payload.len() == 1)
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
    profile::validate(&profile)
        .map_err(|profile::Invalid(message)| (ErrorCode::InvalidProfile, message))?;
    Ok(profile)
}

/// Why a profile write failed: a stable label, safe to log and to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreFailed(pub &'static str);

/// Where profiles are written. A trait because Secrets Manager is a real
/// external boundary: production uses [`SecretsManagerStore`], tests an
/// in-memory store.
pub trait ProfileStore: Send + Sync {
    /// Write `profile` as the new current version of the secret `arn` in
    /// `region`, and return the version id.
    fn put<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
        profile: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, StoreFailed>> + Send + 'a>>;
}

/// Secrets Manager `PutSecretValue` with the ambient credentials (IRSA
/// in-cluster). It writes no files.
pub struct SecretsManagerStore {
    shared: aws_config::SdkConfig,
}

impl SecretsManagerStore {
    pub async fn new() -> Self {
        let timeouts = aws_config::timeout::TimeoutConfig::builder()
            .connect_timeout(Duration::from_secs(5))
            .operation_attempt_timeout(Duration::from_secs(10))
            .operation_timeout(Duration::from_secs(25))
            .build();
        let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .timeout_config(timeouts)
            .load()
            .await;
        Self { shared }
    }
}

impl ProfileStore for SecretsManagerStore {
    fn put<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
        profile: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            let config = aws_sdk_secretsmanager::config::Builder::from(&self.shared)
                .region(aws_sdk_secretsmanager::config::Region::new(
                    region.to_string(),
                ))
                .build();
            let output = aws_sdk_secretsmanager::Client::from_conf(config)
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
}

impl Enrollment {
    pub fn new(github: GitHub, store: Arc<dyn ProfileStore>) -> Self {
        Self { github, store }
    }

    /// Check the token's GitHub login, then write the profile to the
    /// developer's secret and answer with the new version id. The profile is
    /// dropped, and its serialized copy zeroized, before this returns.
    pub async fn run(&self, developer: &Developer, profile: Value) -> Value {
        // Both are strings: the profile passed validation at admission.
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
        let text = Zeroizing::new(profile.to_string());
        drop(profile);
        match self
            .store
            .put(&developer.profile_secret_arn, &developer.region, &text)
            .await
        {
            Ok(version_id) => serde_json::json!({"ok": true, "version_id": version_id}),
            Err(StoreFailed(label)) => refusal(
                ErrorCode::ProfileStoreFailed,
                &format!("could not store the profile ({label}); retry"),
            ),
        }
    }
}

#[cfg(test)]
#[path = "enroll_tests.rs"]
pub(crate) mod tests;
