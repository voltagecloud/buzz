//! One bounded ciphertext upload, after the caller confirms its durable intent.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, ensure, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use buzz_sdk::sandbox_attach::snapshot::MAX_BLOB_BYTES;
use nostr::hashes::{sha256, Hash};
use nostr::{EventBuilder, JsonUtil, Kind, Tag, Timestamp};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::relay::RestClient;

const DEADLINE: Duration = Duration::from_secs(120);
const RESPONSE_BYTES: usize = 4096;

#[derive(Deserialize)]
struct Uploaded {
    sha256: String,
    size: u64,
}

/// Upload exactly one immutable, hash-verified ciphertext body to this community.
/// The caller must confirm its durable intent first and cancel on authority loss.
/// This operation never retries, follows redirects, or promises physical deletion.
pub(super) async fn put(
    rest: &RestClient,
    ciphertext: &Path,
    expected_hash: &str,
    expected_size: u64,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        ensure!(
            expected_hash.len() == 64
                && expected_hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                && (26..=MAX_BLOB_BYTES as u64).contains(&expected_size),
            "invalid snapshot upload bounds"
        );
        let url = url::Url::parse(&format!("{}/upload", rest.base_url.trim_end_matches('/')))
            .map_err(|_| anyhow!("invalid snapshot upload relay"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid snapshot upload relay"
        );
        let server = buzz_core::tenant::relay_url_authority(rest.base_url.as_str());
        ensure!(!server.is_empty(), "invalid snapshot upload community");

        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(ciphertext)
            .await
            .map_err(|_| anyhow!("snapshot ciphertext file unavailable"))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|_| anyhow!("snapshot ciphertext metadata unavailable"))?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == nix::unistd::geteuid().as_raw()
                && metadata.permissions().mode() & 0o077 == 0
                && metadata.len() == expected_size,
            "snapshot ciphertext must be a private regular file of the approved size"
        );
        let mut body = Vec::new();
        body.try_reserve_exact(expected_size as usize + 1)
            .map_err(|_| anyhow!("snapshot ciphertext memory budget unavailable"))?;
        file.take(expected_size + 1)
            .read_to_end(&mut body)
            .await
            .map_err(|_| anyhow!("snapshot ciphertext read failed"))?;
        ensure!(
            body.len() as u64 == expected_size
                && sha256::Hash::hash(&body).to_string() == expected_hash,
            "snapshot ciphertext differs from its durable upload intent"
        );

        let created_at = Timestamp::now();
        let expires = created_at.as_secs().saturating_add(55).to_string();
        let tags = [
            ["t", "upload"],
            ["x", expected_hash],
            ["server", server.as_str()],
            ["expiration", expires.as_str()],
        ]
        .into_iter()
        .map(Tag::parse)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| anyhow!("snapshot upload proof construction failed"))?;
        let proof = EventBuilder::new(Kind::from(24242), "Upload encrypted snapshot")
            .tags(tags)
            .custom_created_at(created_at)
            .sign_with_keys(&rest.keys)
            .map_err(|_| anyhow!("snapshot upload proof signing failed"))?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| anyhow!("snapshot upload client unavailable"))?;
        let mut request = http
            .put(url)
            .header(
                "Authorization",
                format!(
                    "Nostr {}",
                    URL_SAFE_NO_PAD.encode(proof.as_json().as_bytes())
                ),
            )
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", expected_hash)
            .body(body);
        if let Some(tag) = &rest.auth_tag_json {
            request = request.header("x-auth-tag", tag);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| anyhow!("snapshot upload request failed; acceptance is unknown"))?;
        ensure!(
            response.status().is_success(),
            "snapshot upload refused with HTTP {}",
            response.status().as_u16()
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= RESPONSE_BYTES as u64),
            "snapshot upload response exceeds metadata budget"
        );
        let mut metadata = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            anyhow!("snapshot upload receipt was interrupted; acceptance is unknown")
        })? {
            ensure!(
                chunk.len() <= RESPONSE_BYTES - metadata.len(),
                "snapshot upload response exceeds metadata budget"
            );
            metadata.extend_from_slice(&chunk);
        }
        let uploaded: Uploaded = serde_json::from_slice(&metadata)
            .map_err(|_| anyhow!("invalid snapshot upload receipt; acceptance is unknown"))?;
        ensure!(
            uploaded.sha256 == expected_hash && uploaded.size == expected_size,
            "snapshot upload receipt differs from its durable intent"
        );
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("snapshot upload deadline exceeded; acceptance is unknown"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, Bytes},
        http::HeaderMap,
        response::Response,
        routing::put as route_put,
        Json, Router,
    };
    use futures_util::{stream, StreamExt};
    use nostr::{Event, Keys};
    use serde_json::json;
    use std::convert::Infallible;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct Artifact {
        _directory: tempfile::TempDir,
        file: std::path::PathBuf,
        bytes: Vec<u8>,
        hash: String,
    }
    impl Artifact {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("ciphertext");
            let bytes = vec![42; 64];
            std::fs::write(&file, &bytes).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            let hash = sha256::Hash::hash(&bytes).to_string();
            Self {
                _directory: directory,
                file,
                bytes,
                hash,
            }
        }
    }

    async fn fixture(app: Router) -> (RestClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            RestClient {
                http: reqwest::Client::new(),
                base_url,
                keys: Keys::generate(),
                auth_tag_json: Some(r#"["auth","fixture-owner"]"#.into()),
            },
            server,
        )
    }

    #[tokio::test]
    async fn exact_ciphertext_is_uploaded_once_with_hash_bound_community_proof() {
        let artifact = Artifact::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let expected = artifact.bytes.clone();
        let hash = artifact.hash.clone();
        let keys = Keys::generate();
        let signer = keys.public_key();
        let (mut rest, server) = fixture(Router::new().route(
            "/upload",
            route_put(move |headers: HeaderMap, body: Bytes| {
                let (expected, hash, seen) = (expected.clone(), hash.clone(), seen.clone());
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(body.as_ref(), expected);
                    assert_eq!(headers["x-sha-256"], hash);
                    assert_eq!(headers["content-type"], "application/octet-stream");
                    assert_eq!(headers["x-auth-tag"], r#"["auth","fixture-owner"]"#);
                    let encoded = headers["authorization"]
                        .to_str()
                        .unwrap()
                        .strip_prefix("Nostr ")
                        .unwrap();
                    let proof: Event =
                        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
                    proof.verify().unwrap();
                    assert_eq!(proof.kind.as_u16(), 24242);
                    assert_eq!(proof.pubkey, signer);
                    for tag in [vec!["t", "upload"], vec!["x", hash.as_str()]] {
                        assert!(proof.tags.iter().any(|value| value.as_slice() == tag));
                    }
                    let host = headers["host"].to_str().unwrap();
                    assert!(proof.tags.iter().any(|tag| tag.as_slice() == ["server", host]));
                    let expiry = proof
                        .tags
                        .iter()
                        .find(|tag| tag.as_slice().first().is_some_and(|key| key == "expiration"))
                        .unwrap()
                        .as_slice()[1]
                        .parse::<u64>()
                        .unwrap();
                    assert_eq!(expiry - proof.created_at.as_secs(), 55);
                    Json(json!({"sha256":hash,"size":body.len(),"url":"https://unused.invalid/ignored"}))
                }
            }),
        ))
        .await;
        rest.keys = keys;
        put(&rest, &artifact.file, &artifact.hash, 64)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&artifact.file).unwrap(), artifact.bytes);
        server.abort();
    }

    #[tokio::test]
    async fn invalid_or_nonprivate_local_bytes_are_refused_before_any_put() {
        let artifact = Artifact::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let (rest, server) = fixture(Router::new().fallback(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async { Json(json!({})) }
        }))
        .await;
        for (hash, size) in [
            ("a".repeat(64), 64),
            (artifact.hash.clone(), 65),
            (artifact.hash.clone(), MAX_BLOB_BYTES as u64 + 1),
        ] {
            assert!(put(&rest, &artifact.file, &hash, size).await.is_err());
        }
        std::fs::set_permissions(&artifact.file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(put(&rest, &artifact.file, &artifact.hash, 64)
            .await
            .is_err());
        std::fs::set_permissions(&artifact.file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = artifact.file.with_extension("link");
        std::os::unix::fs::symlink(&artifact.file, &link).unwrap();
        assert!(put(&rest, &link, &artifact.hash, 64).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn mismatched_receipts_and_http_failure_never_retry_or_reveal_server_diagnostics() {
        let artifact = Artifact::new();
        for (status, body) in [
            (200, json!({"sha256":"0".repeat(64),"size":64}).to_string()),
            (200, json!({"sha256":artifact.hash,"size":63}).to_string()),
            (200, "fixture-private-server-diagnostic".into()),
            (503, "fixture-private-server-diagnostic".into()),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let (rest, server) = fixture(Router::new().route(
                "/upload",
                route_put(move || {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let body = body.clone();
                    async move {
                        Response::builder()
                            .status(status)
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            ))
            .await;
            let error = put(&rest, &artifact.file, &artifact.hash, 64)
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("fixture-private"));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn redirects_cannot_forward_proofs_or_ciphertext() {
        let artifact = Artifact::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let (destination, remote) = fixture(Router::new().fallback(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async { Json(json!({})) }
        }))
        .await;
        let (rest, server) = fixture(Router::new().route(
            "/upload",
            route_put(move || {
                let location = destination.base_url.clone();
                async move {
                    Response::builder()
                        .status(307)
                        .header("location", location)
                        .body(Body::empty())
                        .unwrap()
                }
            }),
        ))
        .await;
        assert!(put(&rest, &artifact.file, &artifact.hash, 64)
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
        remote.abort();
    }

    #[tokio::test]
    async fn response_bounds_refuse_oversized_headers_and_streams_before_eof() {
        let artifact = Artifact::new();
        for header in [false, true] {
            let (rest, server) = fixture(Router::new().route(
                "/upload",
                route_put(move || async move {
                    let payload = if header {
                        vec![b'x']
                    } else {
                        vec![b'x'; RESPONSE_BYTES + 1]
                    };
                    let body = Body::from_stream(
                        stream::once(async { Ok::<_, Infallible>(Bytes::from(payload)) })
                            .chain(stream::pending()),
                    );
                    let mut response = Response::builder();
                    if header {
                        response = response.header("content-length", RESPONSE_BYTES + 1);
                    }
                    response.body(body).unwrap()
                }),
            ))
            .await;
            let error = tokio::time::timeout(
                Duration::from_secs(2),
                put(&rest, &artifact.file, &artifact.hash, 64),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(error.to_string().contains("metadata budget"));
            server.abort();
        }
    }

    #[tokio::test]
    async fn stalled_upload_receipt_has_a_total_deadline() {
        let artifact = Artifact::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let seen = started.clone();
        let (rest, server) = fixture(Router::new().route(
            "/upload",
            route_put(move || {
                let seen = seen.clone();
                async move {
                    seen.notify_one();
                    Body::from_stream(
                        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"{")) })
                            .chain(stream::pending()),
                    )
                }
            }),
        ))
        .await;
        let job = tokio::spawn(async move { put(&rest, &artifact.file, &artifact.hash, 64).await });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        let error = tokio::time::timeout(Duration::from_secs(1), job)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("deadline exceeded"));
        tokio::time::resume();
        server.abort();
    }
}
