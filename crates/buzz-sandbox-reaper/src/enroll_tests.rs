//! Enrollment steps 5 and 6 against a fake GitHub API and an in-memory
//! secret store. The admission refusals are in `manager_tests.rs`, and the
//! relay round trip in `serve_tests.rs`.

use super::*;
use std::sync::Mutex;

use axum::http::{HeaderMap, StatusCode};
use serde_json::json;

/// What a write received: ARN, region and secret string.
pub(crate) type Written = Arc<Mutex<Vec<(String, String, String)>>>;

/// An in-memory secret store that records every write, or fails each one.
pub(crate) struct FakeStore {
    pub(crate) written: Written,
    pub(crate) fail: Option<&'static str>,
}

impl ProfileStore for FakeStore {
    /// The last write is the current version.
    fn get<'a>(
        &'a self,
        _arn: &'a str,
        _region: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Stored>, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(label) = self.fail {
                return Err(StoreFailed(label));
            }
            let written = self.written.lock().unwrap();
            Ok(written.last().map(|(_, _, text)| Stored {
                text: Zeroizing::new(text.clone()),
                version_id: format!("version-{}", written.len()),
                created: Some("2026-10-23T12:00:00Z".into()),
            }))
        })
    }

    fn put<'a>(
        &'a self,
        arn: &'a str,
        region: &'a str,
        profile: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(label) = self.fail {
                return Err(StoreFailed(label));
            }
            let mut written = self.written.lock().unwrap();
            written.push((arn.into(), region.into(), profile.into()));
            Ok(format!("version-{}", written.len()))
        })
    }
}

/// A fake `GET /user` that maps bearer tokens to logins; any other token
/// gets 401.
pub(crate) async fn fake_github(logins: &[(&str, &str)]) -> String {
    let logins: std::collections::HashMap<String, String> = logins
        .iter()
        .map(|(token, login)| (format!("Bearer {token}"), login.to_string()))
        .collect();
    let app = axum::Router::new().route(
        "/user",
        axum::routing::get(move |headers: HeaderMap| {
            let login = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| logins.get(value).cloned());
            async move {
                match login {
                    Some(login) => (StatusCode::OK, json!({"login": login, "id": 1}).to_string()),
                    None => (
                        StatusCode::UNAUTHORIZED,
                        "{\"message\":\"Bad credentials\"}".into(),
                    ),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// An enrollment whose GitHub is unreachable and whose store records writes.
pub(crate) fn offline(written: Written) -> Enrollment {
    Enrollment::new(
        GitHub::new("http://127.0.0.1:9").unwrap(),
        Arc::new(FakeStore {
            written,
            fail: None,
        }),
    )
}

/// The developer `dev` as the config file declares it.
fn developer() -> Developer {
    Developer {
        id: "dev".into(),
        namespace: "buzz-agent-staging-dev".into(),
        profile_secret_arn:
            "arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"
                .into(),
        region: "us-west-2".into(),
    }
}

fn profile() -> Value {
    crate::profile::tests::valid("dev", &"a".repeat(64))
}

async fn enroll(github: &[(&str, &str)], fail: Option<&'static str>) -> (Value, Written) {
    let written = Written::default();
    let enrollment = Enrollment::new(
        GitHub::new(&fake_github(github).await).unwrap(),
        Arc::new(FakeStore {
            written: written.clone(),
            fail,
        }),
    );
    (enrollment.run(&developer(), profile()).await, written)
}

/// Step 6: the profile JSON, exactly, to the configured ARN and its region.
#[tokio::test]
async fn a_verified_profile_is_written_to_the_developers_secret() {
    // GitHub logins are case-insensitive.
    let (response, written) = enroll(&[("ghp_fixtureGitHubTokenDoNotLeak", "Ada-L")], None).await;
    assert_eq!(response["ok"], true);
    assert_eq!(response["version_id"], "version-1");
    assert_eq!(
        response["profile"]["github"],
        json!({"login": "ada-l", "token": true})
    );
    let written = written.lock().unwrap();
    assert_eq!(written.len(), 1);
    let (arn, region, text) = &written[0];
    assert_eq!(arn, &developer().profile_secret_arn);
    assert_eq!(region, "us-west-2");
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), profile());
}

/// Step 5: the token must belong to the profile's login, and nothing is
/// written otherwise.
#[tokio::test]
async fn a_token_for_another_login_or_none_is_refused_before_the_write() {
    for (github, message) in [
        (
            &[("ghp_fixtureGitHubTokenDoNotLeak", "someone-else")][..],
            "different login",
        ),
        (&[][..], "refused the profile's token"),
    ] {
        let (response, written) = enroll(github, None).await;
        assert_eq!(response["code"], "github_login_mismatch", "{response}");
        assert!(response["error"].as_str().unwrap().contains(message));
        assert!(written.lock().unwrap().is_empty());
    }
    let response = offline(Written::default())
        .run(&developer(), profile())
        .await;
    assert_eq!(response["code"], "github_login_mismatch");
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("could not reach GitHub"));
}

#[tokio::test]
async fn a_failed_write_is_profile_store_failed() {
    let (response, _) = enroll(
        &[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")],
        Some("access_denied"),
    )
    .await;
    assert_eq!(response["code"], "profile_store_failed");
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("access_denied"));
}

#[test]
fn the_github_api_must_be_https_unless_loopback() {
    assert!(GitHub::new(DEFAULT_GITHUB_API_URL).is_ok());
    assert!(GitHub::new("http://127.0.0.1:8080/api/v3/").is_ok());
    assert!(GitHub::new("http://github.example.com").is_err());
    assert!(GitHub::new("not a url").is_err());
}

/// A profile without a developer ID is stored under the signing owner's.
#[test]
fn a_profile_without_a_developer_gets_the_owners() {
    let owner = nostr::Keys::generate().public_key();
    let mut sent = profile();
    sent.as_object_mut().unwrap().remove("developer");
    sent["owner_pubkey"] = json!(owner.to_hex());
    let admitted = admit_profile(&json!({"profile": sent}), &owner, &developer()).unwrap();
    assert_eq!(admitted["developer"], "dev");
}

/// An enrollment with a fake GitHub that knows `tokens`, and the store.
async fn enrollment(tokens: &[(&str, &str)]) -> (Enrollment, Written) {
    let written = Written::default();
    let enrollment = Enrollment::new(
        GitHub::new(&fake_github(tokens).await).unwrap(),
        Arc::new(FakeStore {
            written: written.clone(),
            fail: None,
        }),
    );
    (enrollment, written)
}

fn stored(written: &Written) -> Value {
    serde_json::from_str(&written.lock().unwrap().last().unwrap().2).unwrap()
}

/// A partial profile keeps what it does not name, and needs no GitHub check
/// when it carries no new token.
#[tokio::test]
async fn a_partial_profile_keeps_every_field_it_does_not_name() {
    // GitHub knows the first token only: the partial enroll must not ask it.
    let (enrollment, written) = enrollment(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    assert_eq!(enrollment.run(&developer(), profile()).await["ok"], true);
    let partial = json!({"version": 1, "developer": "dev", "owner_pubkey": "a".repeat(64),
        "models": {"codex_access_token": "at-fixtureCodexTokenDoNotLeak"}});
    let response = enrollment.run(&developer(), partial).await;
    assert_eq!(response["ok"], true, "{response}");
    let mut expected = profile();
    expected["models"]["codex_access_token"] = json!("at-fixtureCodexTokenDoNotLeak");
    assert_eq!(stored(&written), expected);
    assert_eq!(
        response["profile"]["models"],
        json!(["anthropic_api_key", "codex_access_token"])
    );
}

/// `null` removes a model credential, and setting one of an exclusive pair
/// replaces the other.
#[tokio::test]
async fn null_removes_a_model_credential_and_a_pair_replaces_its_other() {
    let (enrollment, written) = enrollment(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    enrollment.run(&developer(), profile()).await;
    let owner = "a".repeat(64);
    let partial = |models: Value| json!({"version": 1, "developer": "dev", "owner_pubkey": owner, "models": models});
    enrollment
        .run(
            &developer(),
            partial(json!({"claude_oauth_token": "sk-ant-oat01-fixture"})),
        )
        .await;
    assert_eq!(
        stored(&written)["models"],
        json!({"claude_oauth_token": "sk-ant-oat01-fixture"})
    );
    enrollment
        .run(&developer(), partial(json!({"claude_oauth_token": null})))
        .await;
    assert_eq!(stored(&written)["models"], json!({}));
}

/// A new token is checked against GitHub, and the merged profile is
/// validated, before anything is written.
#[tokio::test]
async fn a_new_token_and_the_merged_profile_are_checked_before_the_write() {
    let (enrollment, written) = enrollment(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    enrollment.run(&developer(), profile()).await;
    let owner = "a".repeat(64);
    let response = enrollment
        .run(
            &developer(),
            json!({"version": 1, "developer": "dev", "owner_pubkey": owner,
            "github": {"login": "ada-l", "token": "ghp_unknownToGitHub"}}),
        )
        .await;
    assert_eq!(response["code"], "github_login_mismatch", "{response}");
    let response = enrollment
        .run(
            &developer(),
            json!({"version": 1, "developer": "dev", "owner_pubkey": owner,
            "git": {"email": "not\nan email"}}),
        )
        .await;
    assert_eq!(response["error"], "Invalid Git identity", "{response}");
    assert_eq!(written.lock().unwrap().len(), 1);
}

/// With nothing stored, a partial profile is refused.
#[tokio::test]
async fn a_first_enroll_must_send_the_complete_profile() {
    let (enrollment, written) = enrollment(&[]).await;
    let response = enrollment
        .run(
            &developer(),
            json!({"version": 1, "developer": "dev", "owner_pubkey": "a".repeat(64),
            "models": {"codex_access_token": "at-fixture"}}),
        )
        .await;
    assert_eq!(response["code"], "invalid_profile");
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("send the complete profile"));
    assert!(written.lock().unwrap().is_empty());
}

/// `profile` describes the stored profile, naming no credential value.
#[tokio::test]
async fn describe_names_what_is_stored_and_no_credential() {
    let (enrollment, _) = enrollment(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    assert_eq!(
        enrollment.describe(&developer()).await,
        json!({"ok": true, "profile": null})
    );
    enrollment.run(&developer(), profile()).await;
    let described = enrollment.describe(&developer()).await;
    assert_eq!(
        described,
        json!({"ok": true, "profile": {
            "version_id": "version-1",
            "updated_at": "2026-10-23T12:00:00Z",
            "git": {"name": "Ada Lovelace", "email": "ada@example.com", "signing_key": true},
            "github": {"login": "ada-l", "token": true},
            "via": {"profile": true, "op_token": true},
            "models": ["anthropic_api_key"],
        }})
    );
    let text = described.to_string();
    for secret in ["DoNotLeak", "PRIVATE KEY", "providers.op"] {
        assert!(!text.contains(secret), "{secret}");
    }
}
