//! Enrollment steps 5 and 6 against a fake GitHub API and an in-memory
//! secret store. The admission refusals are in `manager_tests.rs`, and the
//! relay round trip in `serve_tests.rs`.

use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;

use axum::http::{HeaderMap, StatusCode};
use base64::{engine::general_purpose::STANDARD, Engine};
use buzz_backend_kubernetes::sandbox_fake;
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

/// The developer's daemon Secret in the fake cluster, by key, decoded.
fn daemon_secret(cluster: &sandbox_fake::Shared) -> Option<(Value, BTreeMap<String, String>)> {
    let cluster = cluster.lock().unwrap();
    let secret = cluster.secrets.get("paseo-daemon-dev")?;
    let data = secret["data"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| {
            let bytes = STANDARD.decode(value.as_str().unwrap()).unwrap();
            (key.clone(), String::from_utf8(bytes).unwrap())
        })
        .collect();
    Some((secret["metadata"].clone(), data))
}

/// The test relay's daemon Secrets, in `cluster`.
pub(crate) fn daemon_secrets(cluster: &sandbox_fake::Shared) -> DaemonSecrets {
    DaemonSecrets::new(
        sandbox_fake::client(cluster),
        paseo::RELAY_ENDPOINT,
        "wss://relay.example.invalid",
    )
    .unwrap()
}

/// With Paseo daemons on, the first enroll generates a pairing and stores
/// only in the narrow Secret. A failed daemon Secret write is reported, never a
/// silent success, and stores no profile. Later enrolls keep the pairing, so paired apps
/// survive, writes the Secret with the daemon's environment only (the
/// developer's registered agents, and of the model credentials only the
/// Claude subscription token), and returns the password and offer; so does
/// `profile`. Replacing the Secret works too.
#[tokio::test]
async fn paseo_pairing_is_kept_only_in_the_daemon_secret_and_returned() {
    use crate::mobile_launch::Registration;
    use nostr::nips::nip19::ToBech32;

    let cluster = sandbox_fake::Shared::default();
    let (owner, agent) = (
        nostr::Keys::parse(&"33".repeat(32)).unwrap(),
        nostr::Keys::parse(&"22".repeat(32)).unwrap(),
    );
    let channel = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10";
    let launch = json!({"relay_url": "wss://relay.example.invalid",
        "private_key_nsec": agent.secret_key().to_bech32().unwrap(),
        "launch": {"policy_env": {"BUZZ_ACP_DISPLAY_NAME": "Brokkr"}}});
    Registration::new(owner.public_key(), agent.public_key(), launch)
        .unwrap()
        .in_channel(&json!({"sandbox": {"channel_id": channel, "thread_root": "b".repeat(64)}}))
        .save(sandbox_fake::client(&cluster), "buzz-agent-staging-dev")
        .await
        .unwrap();
    // Another community's registration is not this daemon's.
    let elsewhere = nostr::Keys::parse(&"55".repeat(32)).unwrap();
    Registration::new(
        owner.public_key(),
        elsewhere.public_key(),
        json!({"relay_url": "wss://other.example.invalid",
            "private_key_nsec": elsewhere.secret_key().to_bech32().unwrap()}),
    )
    .unwrap()
    .in_channel(&json!({"sandbox": {"channel_id": channel, "thread_root": "b".repeat(64)}}))
    .save(sandbox_fake::client(&cluster), "buzz-agent-staging-dev")
    .await
    .unwrap();
    cluster.lock().unwrap().fail_secret_creates = 1;
    let (enrollment, written) = enrollment(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    let enrollment = enrollment.with_paseo(daemon_secrets(&cluster));
    assert_eq!(
        enrollment.describe(&developer()).await["paseo_enabled"],
        true,
        "enabled provisioning is advertised before a daemon Secret exists"
    );

    // A refusal leaves the profile unchanged, so a client that keeps what
    // it sent (a new signing key) for its retry never strands a stored one.
    let response = enrollment.run(&developer(), profile()).await;
    assert_eq!(response["code"], "paseo_store_failed", "{response}");
    assert!(response["error"].as_str().unwrap().contains("unchanged"));
    assert!(written.lock().unwrap().is_empty());
    assert!(daemon_secret(&cluster).is_none());
    let response = enrollment.run(&developer(), profile()).await;
    assert_eq!(response["version_id"], "version-1", "{response}");
    assert!(stored(&written).get("paseo").is_none());
    let initial = daemon_secret(&cluster).unwrap().1;
    let pairing = json!({"password": initial["PASEO_PASSWORD"], "server_id": initial["PASEO_SERVER_ID"],
        "daemon_keypair": serde_json::from_str::<Value>(&initial["PASEO_DAEMON_KEYPAIR"]).unwrap()});
    assert_eq!(
        daemon_secret(&cluster).unwrap().1["PASEO_PASSWORD"],
        pairing["password"].as_str().unwrap()
    );

    // A failed Secret read must not generate a new identity or write a profile.
    cluster.lock().unwrap().fail_secret_reads = 1;
    let refused = enrollment.run(&developer(), profile()).await;
    assert_eq!(refused["code"], "paseo_store_failed", "{refused}");
    assert_eq!(written.lock().unwrap().len(), 1);
    assert_eq!(daemon_secret(&cluster).unwrap().1, initial);

    let partial = json!({"version": 1, "developer": "dev", "owner_pubkey": "a".repeat(64),
        "models": {"claude_oauth_token": "sk-ant-oat01-fixture"}});
    for version in ["version-2", "version-3"] {
        let response = enrollment.run(&developer(), partial.clone()).await;
        assert_eq!(response["version_id"], version, "{response}");
        assert!(stored(&written).get("paseo").is_none());
        let (metadata, env) = daemon_secret(&cluster).unwrap();
        assert_eq!(metadata["namespace"], "buzz-paseo");
        let agents = json!([{"id": "brokkr", "label": "Brokkr",
            "agent": agent.public_key().to_hex(), "channel": channel}]);
        assert_eq!(
            env,
            BTreeMap::from([
                (
                    "PASEO_PASSWORD".into(),
                    pairing["password"].as_str().unwrap().into()
                ),
                (
                    "PASEO_SERVER_ID".into(),
                    pairing["server_id"].as_str().unwrap().into()
                ),
                (
                    "PASEO_DAEMON_KEYPAIR".into(),
                    pairing["daemon_keypair"].to_string()
                ),
                ("BUZZ_PASEO_AGENTS".into(), agents.to_string()),
                (
                    "BUZZ_PRIVATE_KEY".into(),
                    initial["BUZZ_PRIVATE_KEY"].clone()
                ),
                (
                    "CLAUDE_CODE_OAUTH_TOKEN".into(),
                    "sk-ant-oat01-fixture".into()
                ),
            ])
        );
        let offer = crate::paseo::offer_url(
            pairing["server_id"].as_str().unwrap(),
            pairing["daemon_keypair"]["publicKeyB64"].as_str().unwrap(),
            paseo::RELAY_ENDPOINT,
        );
        let expected = json!({"password": pairing["password"],
            "server_id": pairing["server_id"], "offer_url": offer,
            "gateway_pubkey": nostr::Keys::parse(&initial["BUZZ_PRIVATE_KEY"]).unwrap().public_key().to_hex()});
        assert_eq!(response["paseo"], expected);
        let described = enrollment.describe(&developer()).await;
        assert_eq!(described["paseo"], expected);
        assert_eq!(described["paseo_enabled"], true);
        // The daemon's secret key stays out of every reply.
        let secret = pairing["daemon_keypair"]["secretKeyB64"].as_str().unwrap();
        assert!(!response.to_string().contains(secret));
        assert!(!response.to_string().contains(&initial["BUZZ_PRIVATE_KEY"]));
        assert!(!stored(&written).to_string().contains(secret));
        assert!(!stored(&written)
            .to_string()
            .contains(&initial["BUZZ_PRIVATE_KEY"]));
    }

    // The agent list is derived and capped: more registrations than it holds
    // shorten it, and never block enrollment.
    for index in 0..70u8 {
        let extra = nostr::Keys::parse(&format!("{:064x}", 0x1000 + u32::from(index))).unwrap();
        Registration::new(
            owner.public_key(),
            extra.public_key(),
            json!({"relay_url": "wss://relay.example.invalid",
                "private_key_nsec": extra.secret_key().to_bech32().unwrap()}),
        )
        .unwrap()
        .in_channel(&json!({"sandbox": {"channel_id": channel, "thread_root": "b".repeat(64)}}))
        .save(sandbox_fake::client(&cluster), "buzz-agent-staging-dev")
        .await
        .unwrap();
    }
    let response = enrollment.run(&developer(), partial.clone()).await;
    assert_eq!(response["version_id"], "version-4", "{response}");
    let listed: Vec<Value> =
        serde_json::from_str(&daemon_secret(&cluster).unwrap().1["BUZZ_PASEO_AGENTS"]).unwrap();
    assert_eq!(listed.len(), 64);
}

/// `profile` describes the stored profile, naming no credential value.
#[tokio::test]
async fn describe_names_what_is_stored_and_no_credential() {
    let (enrollment, _) = enrollment(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    assert_eq!(
        enrollment.describe(&developer()).await,
        json!({"ok": true, "developer": "dev", "profile": null, "paseo_enabled": false})
    );
    enrollment.run(&developer(), profile()).await;
    let described = enrollment.describe(&developer()).await;
    assert_eq!(
        described,
        json!({"ok": true, "developer": "dev", "paseo_enabled": false, "profile": {
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

#[tokio::test]
async fn daemon_git_token_rotates_only_in_the_narrow_secret_and_partial_identity_fails_closed() {
    let cluster = sandbox_fake::Shared::default();
    let (enrollment, written) = enrollment(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    let enrollment = enrollment.with_paseo(daemon_secrets(&cluster));
    let partial = json!({"version":1,"owner_pubkey":"a".repeat(64),"developer":"dev"});
    for (index, value) in [
        json!({"github_read_token":"github_pat_FirstReadOnlyFixture"}),
        json!({"github_read_token":"github_pat_RotatedReadOnlyFixture"}),
        Value::Null,
        json!({"github_read_token":null}),
    ]
    .into_iter()
    .enumerate()
    {
        let change = paseo::GitReadToken::parse((index != 2).then_some(&value)).unwrap();
        let reply = enrollment
            .run_with_daemon(
                &developer(),
                if index == 0 {
                    profile()
                } else {
                    partial.clone()
                },
                None,
                change,
            )
            .await;
        assert_eq!(reply["ok"], true, "{reply}");
        let env = daemon_secret(&cluster).unwrap().1;
        match index {
            0 => assert_eq!(
                env[paseo::GIT_READ_TOKEN_KEY],
                "github_pat_FirstReadOnlyFixture"
            ),
            1 | 2 => assert_eq!(
                env[paseo::GIT_READ_TOKEN_KEY],
                "github_pat_RotatedReadOnlyFixture"
            ),
            _ => assert!(!env.contains_key(paseo::GIT_READ_TOKEN_KEY)),
        }
        assert!(!stored(&written).to_string().contains("github_pat_"));
        assert!(!reply.to_string().contains("github_pat_"));
    }
    let original = daemon_secret(&cluster).unwrap().1;
    for key in [
        "PASEO_PASSWORD",
        "PASEO_SERVER_ID",
        "PASEO_DAEMON_KEYPAIR",
        "BUZZ_PRIVATE_KEY",
    ] {
        let broken = {
            let mut state = cluster.lock().unwrap();
            let secret = state.secrets.get_mut("paseo-daemon-dev").unwrap();
            secret["data"].as_object_mut().unwrap().remove(key);
            secret.clone()
        };
        let before = written.lock().unwrap().len();
        let reply = enrollment.run(&developer(), partial.clone()).await;
        assert_eq!(reply["code"], "paseo_store_failed", "{key}: {reply}");
        assert_eq!(written.lock().unwrap().len(), before);
        assert_eq!(
            daemon_secrets(&cluster).refresh_agents(&developer()).await,
            Err(paseo::Failed("invalid_pairing"))
        );
        let mut state = cluster.lock().unwrap();
        assert_eq!(state.secrets["paseo-daemon-dev"], broken);
        state.secrets.get_mut("paseo-daemon-dev").unwrap()["data"][key] =
            Value::String(STANDARD.encode(&original[key]));
    }
}
