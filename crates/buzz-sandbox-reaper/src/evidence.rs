//! Checkpoint evidence: what S3 confirms about a session's checkpoint.
//!
//! A key is recorded on a tombstone only when S3 lists it under the session's
//! own prefix, `<developer prefix><generation>/`. That prefix comes from the
//! reaper's config and the Sandbox's generation annotation, never from the
//! Pod, so a key for another developer or generation is refused before S3 is
//! asked. Three sources name a key:
//!
//! - [`from_receipt`]: the harness's termination-message receipt. The receipt
//!   alone is written by the (untrusted) Pod, so S3 must also list it.
//! - [`latest`]: the newest object in the session prefix, for a session
//!   without a verified receipt (the reconcile engine decides when). It may be
//!   a turn checkpoint older than the final workspace.
//! - [`inherited`]: the provider-written restore key of a recovered session.
//!
//! Any store failure is [`Evidence::Unavailable`]: the caller defers, and
//! never records `missing` because S3 could not answer.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use buzz_backend_kubernetes::lifecycle::{is_session_key, receipt_key};

use crate::plan::Evidence;

/// Bound a complete listing by elapsed time, not by the session's object count.
const LISTING_TIMEOUT: Duration = Duration::from_secs(25);

/// Why the bucket could not answer: the bounded `kind` label of
/// `buzz_sandbox_reaper_s3_errors_total`, so an outage (`timeout`, `other`)
/// can be told apart from misconfiguration (`access_denied`,
/// `not_found_bucket`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreErrorKind {
    Timeout,
    AccessDenied,
    NotFoundBucket,
    Other,
}

impl StoreErrorKind {
    /// Metric label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::AccessDenied => "access_denied",
            Self::NotFoundBucket => "not_found_bucket",
            Self::Other => "other",
        }
    }
}

/// A failed bucket request.
#[derive(Debug)]
pub struct StoreError {
    pub kind: StoreErrorKind,
    pub detail: String,
}

impl StoreError {
    fn other(detail: String) -> Self {
        Self {
            kind: StoreErrorKind::Other,
            detail,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.detail, self.kind.as_str())
    }
}

/// Read-only view of the checkpoint bucket. A trait because S3 is a real
/// external boundary: production uses [`S3Store`], tests an in-memory bucket.
pub trait CheckpointStore: Send + Sync {
    /// Does exactly `key` exist?
    fn contains(&self, key: &str) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// The most recently written checkpoint object directly under `prefix`
    /// (one session's `<developer prefix><generation>/`).
    fn newest(
        &self,
        prefix: &str,
    ) -> impl Future<Output = Result<Option<String>, StoreError>> + Send;
}

/// S3 store using `ListObjectsV2` only (IAM: `s3:ListBucket`).
pub struct S3Store {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3Store {
    /// Build from the ambient AWS credential chain (IRSA in-cluster).
    pub async fn new(bucket: &str, region: &str) -> Self {
        // Bounded so one slow probe cannot stall a namespace pass (and with
        // it shutdown) for longer than a few attempts.
        let timeouts = aws_config::timeout::TimeoutConfig::builder()
            .connect_timeout(Duration::from_secs(5))
            .operation_attempt_timeout(Duration::from_secs(10))
            .operation_timeout(Duration::from_secs(25))
            .build();
        let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .timeout_config(timeouts)
            .load()
            .await;
        Self::from_client(aws_sdk_s3::Client::new(&shared), bucket)
    }

    fn from_client(client: aws_sdk_s3::Client, bucket: &str) -> Self {
        Self {
            client,
            bucket: bucket.to_string(),
        }
    }
}

fn store_error<E, R>(error: SdkError<E, R>) -> StoreError
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: fmt::Debug,
{
    let kind = match &error {
        SdkError::TimeoutError(_) => StoreErrorKind::Timeout,
        SdkError::DispatchFailure(failure) if failure.is_timeout() => StoreErrorKind::Timeout,
        SdkError::ServiceError(service) => match service.err().code() {
            Some("AccessDenied") => StoreErrorKind::AccessDenied,
            Some("NoSuchBucket") => StoreErrorKind::NotFoundBucket,
            _ => StoreErrorKind::Other,
        },
        _ => StoreErrorKind::Other,
    };
    StoreError {
        kind,
        detail: format!(
            "list checkpoints: {}",
            aws_sdk_s3::error::DisplayErrorContext(error)
        ),
    }
}

impl CheckpointStore for S3Store {
    async fn contains(&self, key: &str) -> Result<bool, StoreError> {
        // S3 lists keys in UTF-8 binary order, and a key sorts before every
        // longer key it prefixes. So the first object listed under `key` is
        // `key` itself when it exists: one request, no pagination.
        let page = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(key)
            .max_keys(1)
            .send()
            .await
            .map_err(store_error)?;
        Ok(page.contents().first().and_then(|object| object.key()) == Some(key))
    }

    async fn newest(&self, prefix: &str) -> Result<Option<String>, StoreError> {
        let mut newest: Option<((i64, u32), String)> = None;
        let mut token: Option<String> = None;
        let deadline = tokio::time::Instant::now() + LISTING_TIMEOUT;
        let mut seen_tokens = HashSet::new();
        loop {
            let request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(token.take())
                .send();
            let page = tokio::time::timeout_at(deadline, request)
                .await
                .map_err(|_| StoreError {
                    kind: StoreErrorKind::Timeout,
                    detail: format!("checkpoint listing of {prefix} exceeded its time budget"),
                })?
                .map_err(store_error)?;
            for object in page.contents() {
                let (Some(key), Some(at)) = (object.key(), object.last_modified()) else {
                    continue;
                };
                let at = (at.secs(), at.subsec_nanos());
                if is_session_key(key, prefix)
                    && newest
                        .as_ref()
                        .is_none_or(|(best, best_key)| (at, key) > (*best, best_key.as_str()))
                {
                    newest = Some((at, key.to_string()));
                }
            }
            if page.is_truncated() != Some(true) {
                return Ok(newest.map(|(_, key)| key));
            }
            // A truncated page without a way to continue is not an answer.
            let Some(next) = page.next_continuation_token() else {
                return Err(StoreError::other(format!(
                    "checkpoint listing of {prefix} is truncated without a continuation token"
                )));
            };
            if !seen_tokens.insert(next.to_string()) {
                return Err(StoreError::other(format!(
                    "checkpoint listing of {prefix} repeated a continuation token"
                )));
            }
            token = Some(next.to_string());
        }
    }
}

/// Is `generation` a provider-minted generation (32 lowercase hex)?
fn is_generation(generation: &str) -> bool {
    generation.len() == 32
        && generation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The session prefix `<developer prefix><generation>/`, or `None` when the
/// generation is not one the provider could have written. The one generation
/// check for every prefix the reaper lists.
pub fn session_prefix(developer_prefix: &str, generation: &str) -> Option<String> {
    is_generation(generation).then(|| format!("{developer_prefix}{generation}/"))
}

/// Evidence for a terminated container's `message` within `session_prefix`
/// (`<developer prefix><generation>/`).
pub async fn from_receipt<S: CheckpointStore>(
    store: &S,
    message: Option<&str>,
    session_prefix: &str,
) -> Evidence {
    let Some(key) = message.and_then(|m| receipt_key(m, session_prefix)) else {
        return Evidence::Missing;
    };
    probe(store, key, Evidence::Verified).await
}

/// Evidence for a session without a verified receipt: the newest object
/// directly under its own `session_prefix`, as [`Evidence::Listed`]. Nothing
/// listed is [`Evidence::Missing`]; a failed listing is
/// [`Evidence::Unavailable`], never `Missing`.
pub async fn latest<S: CheckpointStore>(store: &S, session_prefix: &str) -> Evidence {
    match store.newest(session_prefix).await {
        Ok(Some(key)) if is_session_key(&key, session_prefix) => Evidence::Listed(key),
        Ok(_) => Evidence::Missing,
        Err(error) => unavailable(&error),
    }
}

/// Evidence for a recovered session that left no verified checkpoint of its
/// own: the checkpoint it was restored from, if that object still exists
/// directly under `<developer prefix><generation>/` for a valid generation.
/// The generation can be an older ancestor than `recovered-from` after
/// repeated failed recovery. `restore` comes from the provider-written
/// Sandbox annotation, never the Pod, so carrying it forward keeps the
/// original work recoverable after a failed restore or stop.
pub async fn inherited<S: CheckpointStore>(
    store: &S,
    restore: &str,
    developer_prefix: &str,
) -> Evidence {
    let prefix = restore
        .strip_prefix(developer_prefix)
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(generation, _)| session_prefix(developer_prefix, generation));
    match prefix {
        Some(prefix) if is_session_key(restore, &prefix) => {
            probe(store, restore.to_string(), Evidence::Inherited).await
        }
        _ => Evidence::Missing,
    }
}

async fn probe<S: CheckpointStore>(
    store: &S,
    key: String,
    found: fn(String) -> Evidence,
) -> Evidence {
    match store.contains(&key).await {
        Ok(true) => found(key),
        Ok(false) => Evidence::Missing,
        Err(error) => unavailable(&error),
    }
}

fn unavailable(error: &StoreError) -> Evidence {
    tracing::warn!(%error, "checkpoint store failed; deferring decision");
    metrics::counter!("buzz_sandbox_reaper_s3_errors_total", "kind" => error.kind.as_str())
        .increment(1);
    Evidence::Unavailable
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// In-memory bucket recording every probed key or listed prefix.
    /// Insertion order stands in for `LastModified`.
    #[derive(Default)]
    pub struct FakeStore {
        pub keys: Vec<String>,
        pub fail: bool,
        pub probes: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn outage(&self) -> Result<(), StoreError> {
            if self.fail {
                return Err(StoreError::other("synthetic outage".into()));
            }
            Ok(())
        }
    }

    impl CheckpointStore for FakeStore {
        async fn contains(&self, key: &str) -> Result<bool, StoreError> {
            self.probes.lock().unwrap().push(key.to_string());
            self.outage()?;
            Ok(self.keys.iter().any(|k| k == key))
        }

        async fn newest(&self, prefix: &str) -> Result<Option<String>, StoreError> {
            self.probes.lock().unwrap().push(prefix.to_string());
            self.outage()?;
            Ok(self
                .keys
                .iter()
                .rev()
                .find(|k| is_session_key(k, prefix))
                .cloned())
        }
    }

    const PREFIX: &str = "dev/0123456789abcdef0123456789abcdef/";

    fn receipt(key: &str) -> String {
        serde_json::json!({"version": 1, "checkpoint": key}).to_string()
    }

    #[tokio::test]
    async fn receipt_and_listing_must_agree() {
        let key = format!("{PREFIX}6f9619ff-8b86-d011-b42d-00cf4fc964ff.tar.gz");
        let store = FakeStore {
            keys: vec![key.clone()],
            ..Default::default()
        };
        assert_eq!(
            from_receipt(&store, Some(&receipt(&key)), PREFIX).await,
            Evidence::Verified(key.clone())
        );
        // Receipt names an object S3 does not have.
        let other = format!("{PREFIX}00000000-0000-0000-0000-000000000000.tar.gz");
        assert_eq!(
            from_receipt(&store, Some(&receipt(&other)), PREFIX).await,
            Evidence::Missing
        );
        // No receipt, even though S3 has a key.
        assert_eq!(from_receipt(&store, None, PREFIX).await, Evidence::Missing);
        assert_eq!(
            from_receipt(&store, Some("garbage"), PREFIX).await,
            Evidence::Missing
        );
    }

    /// A compromised Pod pointing at another generation's (existing) object
    /// is refused without even asking S3.
    #[tokio::test]
    async fn foreign_generation_receipts_are_refused_before_probing() {
        let foreign = "dev/ffffffffffffffffffffffffffffffff/x.tar.gz".to_string();
        let store = FakeStore {
            keys: vec![foreign.clone()],
            ..Default::default()
        };
        assert_eq!(
            from_receipt(&store, Some(&receipt(&foreign)), PREFIX).await,
            Evidence::Missing
        );
        assert!(store.probes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_s3_outage_defers_rather_than_records_missing() {
        let key = format!("{PREFIX}x.tar.gz");
        let store = FakeStore {
            keys: vec![key.clone()],
            fail: true,
            ..Default::default()
        };
        assert_eq!(
            from_receipt(&store, Some(&receipt(&key)), PREFIX).await,
            Evidence::Unavailable
        );
        assert_eq!(latest(&store, PREFIX).await, Evidence::Unavailable);
        assert_eq!(
            store.probes.lock().unwrap().as_slice(),
            [key.as_str(), PREFIX]
        );
    }

    #[tokio::test]
    async fn the_newest_object_in_the_session_prefix_is_listed() {
        let older = format!("{PREFIX}00000000-0000-0000-0000-000000000001.tar.gz");
        let newer = format!("{PREFIX}00000000-0000-0000-0000-000000000002.tar.gz");
        let store = FakeStore {
            keys: vec![
                older,
                newer.clone(),
                "dev/ffffffffffffffffffffffffffffffff/x.tar.gz".into(),
                format!("{PREFIX}nested/x.tar.gz"),
            ],
            ..Default::default()
        };
        assert_eq!(latest(&store, PREFIX).await, Evidence::Listed(newer));
        assert_eq!(
            latest(&FakeStore::default(), PREFIX).await,
            Evidence::Missing
        );
    }

    #[test]
    fn session_prefixes_require_a_provider_generation() {
        let generation = "0123456789abcdef0123456789abcdef";
        assert_eq!(session_prefix("dev/", generation).as_deref(), Some(PREFIX));
        for bad in [
            "",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789abcdef0123456789abcdeg",
            "../3456789abcdef0123456789abcdef0",
            "0123456789abcdef/123456789abcdef",
        ] {
            assert_eq!(session_prefix("dev/", bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_restore_key_is_inherited_only_within_its_developer_prefix() {
        let key = format!("{PREFIX}6f9619ff-8b86-d011-b42d-00cf4fc964ff.tar.gz");
        let store = FakeStore {
            keys: vec![key.clone()],
            ..Default::default()
        };
        assert_eq!(
            inherited(&store, &key, "dev/").await,
            Evidence::Inherited(key.clone())
        );
        // Expired or deleted since the recovery.
        let gone = format!("{PREFIX}00000000-0000-0000-0000-000000000000.tar.gz");
        assert_eq!(inherited(&store, &gone, "dev/").await, Evidence::Missing);
        // Another developer's prefix, an invalid generation, or a nested key
        // is refused before S3 is asked.
        let probes = store.probes.lock().unwrap().len();
        assert_eq!(
            inherited(&store, &key, "another-developer/").await,
            Evidence::Missing
        );
        for bad in [
            "dev/NOT-A-GENERATION/x.tar.gz",
            &format!("{PREFIX}a/x.tar.gz"),
        ] {
            let store = FakeStore {
                keys: vec![bad.to_string()],
                ..Default::default()
            };
            assert_eq!(inherited(&store, bad, "dev/").await, Evidence::Missing);
            assert!(store.probes.lock().unwrap().is_empty(), "{bad}");
        }
        assert_eq!(store.probes.lock().unwrap().len(), probes);
        let outage = FakeStore {
            fail: true,
            ..Default::default()
        };
        assert_eq!(
            inherited(&outage, &key, "dev/").await,
            Evidence::Unavailable
        );
    }

    // ── The real S3 adapter against a local endpoint ──────────────────────

    /// One canned S3 reply; `None` never answers (holds the connection).
    type Reply = Option<(u16, String)>;

    /// Serve `reply(query)` for every request on a local port, recording
    /// each decoded query. Returns the endpoint URL.
    async fn s3_endpoint(
        reply: impl Fn(&str) -> Reply + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (reply, log) = (Arc::new(reply), Arc::clone(&seen));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (reply, log) = (Arc::clone(&reply), Arc::clone(&log));
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&request);
                    let target = head.split_whitespace().nth(1).unwrap_or_default();
                    let query = decode(target.split_once('?').map_or("", |(_, q)| q));
                    log.lock().unwrap().push(query.clone());
                    let Some((status, body)) = reply(&query) else {
                        std::future::pending::<()>().await;
                        return;
                    };
                    let response = format!(
                        "HTTP/1.1 {status} S3\r\nContent-Type: application/xml\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (url, seen)
    }

    fn decode(query: &str) -> String {
        let mut out = Vec::new();
        let mut bytes = query.bytes();
        while let Some(b) = bytes.next() {
            if b == b'%' {
                let hex: String = bytes.by_ref().take(2).map(char::from).collect();
                out.push(u8::from_str_radix(&hex, 16).unwrap());
            } else {
                out.push(b);
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn page(keys: &[(&str, &str)], next: Option<&str>, truncated: bool) -> Reply {
        let contents: String = keys
            .iter()
            .map(|(key, at)| {
                format!("<Contents><Key>{key}</Key><LastModified>{at}</LastModified><Size>1</Size></Contents>")
            })
            .collect();
        let next = next
            .map(|t| format!("<NextContinuationToken>{t}</NextContinuationToken>"))
            .unwrap_or_default();
        Some((
            200,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Name>bucket</Name><KeyCount>{}</KeyCount><MaxKeys>1000</MaxKeys>\
                 <IsTruncated>{truncated}</IsTruncated>{next}{contents}</ListBucketResult>",
                keys.len()
            ),
        ))
    }

    fn s3_error(status: u16, code: &str) -> Reply {
        Some((
            status,
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>m</Message></Error>"),
        ))
    }

    fn s3(url: &str, timeout: Duration) -> S3Store {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-west-2"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "AKID", "secret", None, None, "test",
            ))
            .endpoint_url(url)
            .force_path_style(true)
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
            .timeout_config(
                aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_timeout(timeout)
                    .build(),
            )
            .build();
        S3Store::from_client(aws_sdk_s3::Client::from_conf(config), "bucket")
    }

    const AT: &str = "2026-10-20T00:00:00.000Z";
    const LATER: &str = "2026-10-20T00:00:01.000Z";

    /// `contains` asks for the key itself as the prefix and accepts only an
    /// exact match, never a longer key that merely starts with it.
    #[tokio::test]
    async fn s3_contains_lists_the_key_as_its_own_prefix() {
        let key = format!("{PREFIX}a.tar.gz");
        let partial = format!("{PREFIX}b.tar.gz");
        let (stored, extended) = (key.clone(), format!("{partial}.tmp"));
        let (url, seen) = s3_endpoint(move |query| {
            if query.contains(&format!("prefix={stored}")) {
                page(&[(&stored, AT)], None, false)
            } else {
                page(&[(&extended, AT)], None, false)
            }
        })
        .await;
        let store = s3(&url, Duration::from_secs(5));
        assert!(store.contains(&key).await.unwrap());
        assert!(!store.contains(&partial).await.unwrap());
        let seen = seen.lock().unwrap();
        assert!(seen.iter().all(|q| q.contains("max-keys=1")), "{seen:?}");
    }

    #[tokio::test]
    async fn s3_newest_follows_continuation_to_a_later_page() {
        let first = format!("{PREFIX}first.tar.gz");
        let second = format!("{PREFIX}second.tar.gz");
        let nested = format!("{PREFIX}nested/x.tar.gz");
        let (a, b) = (first.clone(), second.clone());
        let (url, seen) = s3_endpoint(move |query| {
            if query.contains("continuation-token=page-2") {
                page(
                    &[(&b, LATER), (&nested, "2027-01-01T00:00:00.000Z")],
                    None,
                    false,
                )
            } else {
                page(&[(&a, AT)], Some("page-2"), true)
            }
        })
        .await;
        let store = s3(&url, Duration::from_secs(5));
        assert_eq!(store.newest(PREFIX).await.unwrap(), Some(second));
        assert_eq!(seen.lock().unwrap().len(), 2);
        let (url, _) = s3_endpoint(|_| page(&[], None, false)).await;
        assert_eq!(
            s3(&url, Duration::from_secs(5))
                .newest(PREFIX)
                .await
                .unwrap(),
            None
        );
    }

    /// S3 lists in key order; the newest is the latest `LastModified`, and
    /// the greater key breaks a tie.
    #[tokio::test]
    async fn s3_newest_orders_by_last_modified_then_key() {
        let a = format!("{PREFIX}a.tar.gz");
        let b = format!("{PREFIX}b.tar.gz");
        let z = format!("{PREFIX}z.tar.gz");
        let cases = [
            (vec![(a.clone(), LATER), (z.clone(), AT)], a.clone()),
            (
                vec![(a.clone(), LATER), (b.clone(), LATER), (z, AT)],
                b.clone(),
            ),
            (vec![(b.clone(), LATER), (a.clone(), LATER)], b),
        ];
        for (listed, expected) in cases {
            let (url, _) = s3_endpoint(move |_| {
                let keys: Vec<(&str, &str)> =
                    listed.iter().map(|(k, at)| (k.as_str(), *at)).collect();
                page(&keys, None, false)
            })
            .await;
            let store = s3(&url, Duration::from_secs(5));
            assert_eq!(store.newest(PREFIX).await.unwrap(), Some(expected));
        }
    }

    #[tokio::test]
    async fn s3_newest_can_select_a_checkpoint_after_ten_pages() {
        let key = format!("{PREFIX}last.tar.gz");
        let expected = key.clone();
        let (url, seen) = s3_endpoint(move |query| {
            let index = query
                .split('&')
                .find_map(|part| part.strip_prefix("continuation-token="))
                .map_or(0, |value| value.parse::<usize>().unwrap());
            if index == 11 {
                page(&[(&key, LATER)], None, false)
            } else {
                page(&[], Some(&(index + 1).to_string()), true)
            }
        })
        .await;
        assert_eq!(
            s3(&url, Duration::from_secs(5))
                .newest(PREFIX)
                .await
                .unwrap(),
            Some(expected)
        );
        assert_eq!(seen.lock().unwrap().len(), 12);
    }

    #[tokio::test]
    async fn s3_newest_refuses_partial_listings() {
        // A repeated token is rejected instead of walking forever.
        let (url, seen) = s3_endpoint(|_| page(&[], Some("again"), true)).await;
        let error = s3(&url, Duration::from_secs(5))
            .newest(PREFIX)
            .await
            .unwrap_err();
        assert_eq!(error.kind, StoreErrorKind::Other);
        assert_eq!(seen.lock().unwrap().len(), 2);
        // Truncated with no way to continue is a failure, not "nothing found".
        let (url, _) = s3_endpoint(|_| page(&[], None, true)).await;
        let error = s3(&url, Duration::from_secs(5))
            .newest(PREFIX)
            .await
            .unwrap_err();
        assert!(error.detail.contains("continuation token"), "{error}");
    }

    #[tokio::test]
    async fn s3_failures_are_classified_for_operators() {
        let cases = [
            (s3_error(403, "AccessDenied"), StoreErrorKind::AccessDenied),
            (
                s3_error(404, "NoSuchBucket"),
                StoreErrorKind::NotFoundBucket,
            ),
            (s3_error(500, "InternalError"), StoreErrorKind::Other),
            (s3_error(503, "SlowDown"), StoreErrorKind::Other),
            (None, StoreErrorKind::Timeout),
        ];
        for (reply, kind) in cases {
            let (url, _) = s3_endpoint(move |_| reply.clone()).await;
            let store = s3(&url, Duration::from_millis(300));
            let error = store
                .contains(&format!("{PREFIX}x.tar.gz"))
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "{error}");
            assert_eq!(store.newest(PREFIX).await.unwrap_err().kind, kind);
        }
    }
}
