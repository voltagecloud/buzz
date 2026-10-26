use super::oauth::CLIENT_ID;
use crate::enroll::tests::fake_github;
use crate::enroll::{Enrollment, GitHub};
use crate::profile::tests::valid;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::serve;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::BodyExt;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::{spawn, test as async_test};
use tower::ServiceExt;

use axum::extract::State as RequestState;
use axum::routing::post;
use axum::{Json, Router};

use super::journal::{Journal, JournalResult};
use super::*;
use crate::enroll::{StoreFailed, Stored};

const NOW: i64 = 1_800_000_000;
const CA: &str = "-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----\n";

fn jwt(claims: Value) -> String {
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

fn tokens(account: &str, expires: i64, refresh: &str) -> Tokens {
    Tokens {
        id_token: jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}})),
        access_token: jwt(json!({"exp": expires})),
        refresh_token: refresh.into(),
        account_id: account.into(),
    }
}

fn developer() -> Developer {
    Developer {
        id: "alice".into(),
        namespace: "buzz-agent-staging-alice".into(),
        profile_secret_arn: "profile-alice".into(),
        region: "us-west-2".into(),
    }
}

#[derive(Default)]
struct Store {
    values: StdMutex<HashMap<String, String>>,
    versions: StdMutex<HashMap<(String, String), String>>,
    heads: StdMutex<HashMap<String, String>>,
    puts: AtomicUsize,
    fail_at: AtomicUsize,
    advances: AtomicUsize,
    /// The numbered advance is applied, then its response is lost.
    lose_advance_at: AtomicUsize,
}

impl ProfileStore for Store {
    fn get<'a>(
        &'a self,
        arn: &'a str,
        _region: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Stored>, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            Ok(self.values.lock().unwrap().get(arn).map(|value| Stored {
                text: Zeroizing::new(value.clone()),
                version_id: self.puts.load(Ordering::SeqCst).to_string(),
                created: None,
            }))
        })
    }

    fn get_version<'a>(
        &'a self,
        arn: &'a str,
        _region: &'a str,
        version: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Stored>, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            Ok(self
                .versions
                .lock()
                .unwrap()
                .get(&(arn.to_owned(), version.to_owned()))
                .map(|text| Stored {
                    text: Zeroizing::new(text.clone()),
                    version_id: version.to_owned(),
                    created: None,
                }))
        })
    }

    fn put<'a>(
        &'a self,
        arn: &'a str,
        _region: &'a str,
        value: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, StoreFailed>> + Send + 'a>> {
        Box::pin(async move {
            let number = self.puts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_at.load(Ordering::SeqCst) == number {
                return Err(StoreFailed("unavailable"));
            }
            self.values.lock().unwrap().insert(arn.into(), value.into());
            self.versions
                .lock()
                .unwrap()
                .insert((arn.into(), number.to_string()), value.into());
            Ok(number.to_string())
        })
    }
}

impl Journal for Store {
    fn head<'a>(&'a self, developer: &'a Developer) -> JournalResult<'a, Option<String>> {
        Box::pin(async move { Ok(self.heads.lock().unwrap().get(&developer.id).cloned()) })
    }
    fn advance<'a>(
        &'a self,
        developer: &'a Developer,
        expected: Option<&'a str>,
        revision: &'a str,
    ) -> JournalResult<'a, ()> {
        Box::pin(async move {
            let number = self.advances.fetch_add(1, Ordering::SeqCst) + 1;
            let mut heads = self.heads.lock().unwrap();
            if heads.get(&developer.id).map(String::as_str) != expected {
                return Err(Error::Storage);
            }
            heads.insert(developer.id.clone(), revision.to_owned());
            if self.lose_advance_at.load(Ordering::SeqCst) == number {
                return Err(Error::Storage);
            }
            Ok(())
        })
    }
}

struct Issuer {
    calls: AtomicUsize,
    response: StdMutex<Value>,
    entered: Notify,
    release: Semaphore,
}

struct Upstream {
    endpoint: String,
    issuer: Arc<Issuer>,
    task: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn upstream() -> Upstream {
    let issuer = Arc::new(Issuer {
        calls: AtomicUsize::new(0),
        response: StdMutex::new(json!({
            "access_token": jwt(json!({"exp": NOW + 7200})),
            "id_token": tokens("workspace", NOW + 7200, "unused").id_token,
            "refresh_token": "rotated-upstream-secret",
        })),
        entered: Notify::new(),
        release: Semaphore::new(100),
    });
    async fn refresh(
        RequestState(issuer): RequestState<Arc<Issuer>>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(request["client_id"], CLIENT_ID);
        assert_eq!(request["grant_type"], "refresh_token");
        assert_eq!(request["refresh_token"], "upstream-secret");
        issuer.calls.fetch_add(1, Ordering::SeqCst);
        issuer.entered.notify_one();
        let permit = issuer.release.acquire().await.unwrap();
        permit.forget();
        Json(issuer.response.lock().unwrap().clone())
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/oauth/token", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/oauth/token", post(refresh))
        .with_state(Arc::clone(&issuer));
    let task = spawn(async move { serve(listener, app).await.unwrap() });
    Upstream {
        endpoint,
        issuer,
        task,
    }
}

fn broker(store: Arc<Store>, upstream: &Upstream) -> Arc<Broker> {
    Arc::new(
        Broker::new(
            store.clone(),
            store,
            OAuth::local(&upstream.endpoint).unwrap(),
            [developer()],
            "https://codex-refresh.buzz-sandbox-manager.svc.cluster.local:8443/oauth/token".into(),
            CA.into(),
        )
        .unwrap(),
    )
}

async fn enroll(broker: &Broker, store: &Store, _previous: Option<&Value>) -> Value {
    let developer = developer();
    let _lock = broker.lock(&developer).await.unwrap();
    let metadata = broker
        .stage(
            &developer,
            tokens("workspace", NOW + 3600, "upstream-secret"),
        )
        .await
        .unwrap();
    let profile = json!({"codex_login": metadata});
    store
        .put(
            &developer.profile_secret_arn,
            &developer.region,
            &profile.to_string(),
        )
        .await
        .unwrap();
    broker.activate(&developer, cookie(&profile)).await.unwrap();
    profile
}

fn cookie(profile: &Value) -> &str {
    profile["codex_login"]["refresh_token"].as_str().unwrap()
}

#[test]
fn dedicated_login_validation_refuses_api_keys_and_mismatched_workspaces() {
    let bundle = tokens("workspace", NOW + 3600, "secret");
    let mut login = json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": bundle});
    assert!(Tokens::from_login(&login).is_ok());
    login["auth_mode"] = json!("apikey");
    assert!(matches!(
        Tokens::from_login(&login),
        Err(Error::InvalidLogin)
    ));
    login["auth_mode"] = json!("chatgpt");
    login["tokens"]["account_id"] = json!("other-workspace");
    assert!(matches!(
        Tokens::from_login(&login),
        Err(Error::InvalidLogin)
    ));
    login["tokens"]["account_id"] = json!("workspace");
    login["tokens"]["access_token"] = json!("not-a-jwt");
    assert!(matches!(
        Tokens::from_login(&login),
        Err(Error::InvalidLogin)
    ));
}

#[async_test]
async fn bootstrap_returns_only_short_lived_tokens_without_rotating_a_fresh_login() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    let response = broker.refresh(cookie(&profile), NOW).await.unwrap();
    assert_eq!(response["account_id"], "workspace");
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 0);
    assert!(!response.to_string().contains("upstream-secret"));
    assert!(!profile.to_string().contains("upstream-secret"));
    assert!(response["refresh_token"]
        .as_str()
        .unwrap()
        .starts_with("alice."));
}

#[async_test]
async fn concurrent_sandboxes_and_workers_perform_one_upstream_rotation() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    let seeded = broker.refresh(cookie(&profile), NOW).await.unwrap();
    let credential = seeded["refresh_token"].as_str().unwrap().to_string();
    let mut jobs = JoinSet::new();
    for _ in 0..20 {
        let (broker, credential) = (Arc::clone(&broker), credential.clone());
        jobs.spawn(async move { broker.refresh(&credential, NOW).await });
    }
    let mut responses = Vec::new();
    while let Some(response) = jobs.join_next().await {
        responses.push(response.unwrap().unwrap());
    }
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 1);
    assert!(responses.iter().all(|r| r == &responses[0]));
    assert!(!responses[0].to_string().contains("rotated-upstream-secret"));
    assert_ne!(responses[0]["refresh_token"], credential);
}

#[async_test]
async fn a_fresh_bootstrap_refreshes_an_expired_shared_access_token() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    broker.refresh(cookie(&profile), NOW + 3500).await.unwrap();
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 1);
}

#[async_test]
async fn unknown_developers_and_wrong_credentials_never_reach_the_issuer() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    for credential in [
        "alice.wrong.seed",
        "bob.wrong.seed",
        "",
        "alice.wrong.seed.extra",
    ] {
        assert_eq!(
            broker.refresh(credential, NOW).await.unwrap_err(),
            Error::Unauthorized
        );
    }
    assert!(broker.refresh(cookie(&profile), NOW).await.is_ok());
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 0);
}

#[async_test]
async fn staging_a_new_login_preserves_the_active_login_until_profile_commit() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let old = enroll(&broker, &store, None).await;
    broker.refresh(cookie(&old), NOW).await.unwrap();
    let developer = developer();
    let pending = {
        let _lock = broker.lock(&developer).await.unwrap();
        broker
            .stage(
                &developer,
                tokens("workspace", NOW + 3600, "new-login-secret"),
            )
            .await
            .unwrap()
    };
    assert!(broker.refresh(cookie(&old), NOW).await.is_ok());
    let new = json!({"codex_login": pending});
    assert_eq!(
        broker.refresh(cookie(&new), NOW).await.unwrap_err(),
        Error::Unauthorized
    );
    store
        .put(
            &developer.profile_secret_arn,
            &developer.region,
            &new.to_string(),
        )
        .await
        .unwrap();
    broker.activate(&developer, cookie(&new)).await.unwrap();
    assert_eq!(
        broker.refresh(cookie(&old), NOW).await.unwrap_err(),
        Error::Unauthorized
    );
    let response = broker.refresh(cookie(&new), NOW).await.unwrap();
    assert!(!response.to_string().contains("new-login-secret"));
}

#[async_test]
async fn disconnecting_the_profile_denies_new_runs_even_with_a_valid_cached_bundle() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    let developer = developer();
    store
        .put(&developer.profile_secret_arn, &developer.region, "{}")
        .await
        .unwrap();
    broker.disconnect(&developer).await.unwrap();
    assert_eq!(
        broker.refresh(cookie(&profile), NOW).await.unwrap_err(),
        Error::Unauthorized
    );
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 0);
}

#[async_test]
async fn persistence_failure_before_the_refresh_marker_never_rotates_upstream() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    let seeded = broker.refresh(cookie(&profile), NOW).await.unwrap();
    store
        .fail_at
        .store(store.puts.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    let credential = seeded["refresh_token"].as_str().unwrap();
    assert_eq!(
        broker.refresh(credential, NOW).await.unwrap_err(),
        Error::Storage
    );
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 0);
    assert!(broker.refresh(credential, NOW).await.is_ok());
}

#[async_test]
async fn a_lost_write_after_rotation_leaves_a_durable_marker_across_manager_restart() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let first = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&first, &store, None).await;
    let seeded = first.refresh(cookie(&profile), NOW).await.unwrap();
    store
        .fail_at
        .store(store.puts.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
    let credential = seeded["refresh_token"].as_str().unwrap();
    assert_eq!(
        first.refresh(credential, NOW).await.unwrap_err(),
        Error::Storage
    );
    let restarted = broker(Arc::clone(&store), &upstream);
    assert_eq!(
        restarted.refresh(credential, NOW).await.unwrap_err(),
        Error::Interrupted
    );
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 1);
    let new = enroll(&restarted, &store, Some(&profile)).await;
    assert!(restarted.refresh(cookie(&new), NOW).await.is_ok());
}

/// An SDK retry of an applied conditional update fails its own condition.
/// The refresh marker it published is then owned, not stranded.
#[async_test]
async fn a_lost_advance_response_for_an_applied_update_completes_the_refresh() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let first = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&first, &store, None).await;
    let seeded = first.refresh(cookie(&profile), NOW).await.unwrap();
    store
        .lose_advance_at
        .store(store.advances.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    let credential = seeded["refresh_token"].as_str().unwrap();
    let rotated = first.refresh(credential, NOW).await.unwrap();
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 1);
    // The marker is owned: after a restart the older credential receives the
    // refreshed bundle instead of an interrupted-login refusal.
    let restarted = broker(Arc::clone(&store), &upstream);
    let again = restarted.refresh(credential, NOW).await.unwrap();
    assert_eq!(again["refresh_token"], rotated["refresh_token"]);
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 1);
}

#[async_test]
async fn cancellation_during_upstream_io_cannot_reuse_a_possibly_consumed_token() {
    let upstream = upstream().await;
    upstream.issuer.release.forget_permits(100);
    let store = Arc::new(Store::default());
    let first = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&first, &store, None).await;
    let seeded = first.refresh(cookie(&profile), NOW).await.unwrap();
    let credential = seeded["refresh_token"].as_str().unwrap().to_string();
    let job = {
        let first = Arc::clone(&first);
        let credential = credential.clone();
        spawn(async move { first.refresh(&credential, NOW).await })
    };
    upstream.issuer.entered.notified().await;
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    let restarted = broker(Arc::clone(&store), &upstream);
    assert_eq!(
        restarted.refresh(&credential, NOW).await.unwrap_err(),
        Error::Interrupted
    );
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 1);
}

#[async_test]
async fn an_invalid_upstream_bundle_is_not_persisted_or_echoed() {
    let upstream = upstream().await;
    *upstream.issuer.response.lock().unwrap() = json!({
        "access_token": "synthetic-private-upstream-response",
        "refresh_token": "synthetic-private-rotated-token",
    });
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    let error = broker
        .refresh(cookie(&profile), NOW + 3500)
        .await
        .unwrap_err();
    assert_eq!(error, Error::Upstream);
    assert!(!error.to_string().contains("synthetic-private"));
    assert_eq!(
        broker.refresh(cookie(&profile), NOW).await.unwrap_err(),
        Error::Interrupted
    );
}

#[async_test]
async fn an_uncommitted_secret_version_and_stale_current_value_cannot_bypass_the_journal() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let first = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&first, &store, None).await;
    let seeded = first.refresh(cookie(&profile), NOW).await.unwrap();
    let developer = developer();
    let mut left = first.read(&developer).await.unwrap();
    let mut right = first.read(&developer).await.unwrap();
    left.active.as_mut().unwrap().state = State::Refreshing;
    first.write(&developer, &mut left).await.unwrap();
    // A second manager's stale Ready record writes a secret version but loses CAS.
    assert_eq!(
        first.write(&developer, &mut right).await.unwrap_err(),
        Error::Storage
    );
    // AWSCURRENT is now Ready, but a restart follows the committed marker version.
    let restarted = broker(Arc::clone(&store), &upstream);
    assert_eq!(
        restarted
            .refresh(seeded["refresh_token"].as_str().unwrap(), NOW)
            .await
            .unwrap_err(),
        Error::Interrupted
    );
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 0);
}

#[async_test]
async fn enrollment_activates_only_after_profile_commit_and_revokes_stale_profile_grants() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let developer = developer();
    let github = fake_github(&[("ghp_fixtureGitHubTokenDoNotLeak", "ada-l")]).await;
    let enrollment =
        Enrollment::new(GitHub::new(&github).unwrap(), store.clone()).with_broker(broker.clone());
    let profile = valid("alice", &"a".repeat(64));
    let response = enrollment
        .run_with_login(
            &developer,
            profile.clone(),
            Some(Change::Connect(tokens(
                "workspace",
                NOW + 3600,
                "upstream-secret",
            ))),
        )
        .await;
    assert_eq!(response["ok"], true);
    assert_eq!(response["profile"]["codex_login"], true);
    assert!(!response.to_string().contains("upstream-secret"));
    let stored = store
        .get(&developer.profile_secret_arn, &developer.region)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.text.contains("upstream-secret"));
    let old: Value = from_str(&stored.text).unwrap();
    assert!(broker.refresh(cookie(&old), NOW).await.is_ok());
    store
        .fail_at
        .store(store.puts.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
    let failed = enrollment
        .run_with_login(
            &developer,
            json!({}),
            Some(Change::Connect(tokens(
                "workspace",
                NOW + 3600,
                "replacement-secret",
            ))),
        )
        .await;
    assert_eq!(failed["ok"], false);
    assert!(broker.refresh(cookie(&old), NOW).await.is_ok());
    let disconnected = enrollment
        .run_with_login(&developer, json!({}), Some(Change::Disconnect))
        .await;
    assert_eq!(disconnected["ok"], true);
    // A stale profile replica must not resurrect the disconnected grant.
    store
        .values
        .lock()
        .unwrap()
        .insert(developer.profile_secret_arn.clone(), old.to_string());
    assert_eq!(
        broker.refresh(cookie(&old), NOW).await.unwrap_err(),
        Error::Unauthorized
    );
}

#[async_test]
async fn the_http_boundary_limits_requests_and_never_echoes_credentials() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    let app = server::router(broker);
    for (payload, status) in [
        (
            json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": cookie(&profile)}),
            StatusCode::OK,
        ),
        (
            json!({"client_id": "other-client", "grant_type": "refresh_token", "refresh_token": "private-invalid-cookie"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": "private-invalid-cookie"}),
            StatusCode::UNAUTHORIZED,
        ),
        (
            json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": "x".repeat(2000)}),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let request = Request::post("/oauth/token")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("upstream-secret"));
        assert!(!text.contains("private-invalid-cookie"));
    }
    let request = Request::post("/oauth/token")
        .header("content-type", "application/json")
        .body(Body::from(
            json!("private-malformed-credential").to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8(body.to_vec())
        .unwrap()
        .contains("private-malformed"));
    assert_eq!(upstream.issuer.calls.load(Ordering::SeqCst), 0);
}

/// A request arriving while every permit is held waits for one instead of
/// failing the Sandbox that is bootstrapping.
#[async_test]
async fn a_request_during_a_burst_waits_for_a_permit() {
    let upstream = upstream().await;
    let store = Arc::new(Store::default());
    let broker = broker(Arc::clone(&store), &upstream);
    let profile = enroll(&broker, &store, None).await;
    let permits = Arc::new(Semaphore::new(1));
    let app = server::router_with(broker, Arc::clone(&permits));
    let held = permits.acquire_owned().await.unwrap();
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(held);
    });
    let request = Request::post("/oauth/token")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": cookie(&profile)})
                .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
