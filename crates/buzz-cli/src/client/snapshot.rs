//! Content-addressed, bounded snapshot downloads from the configured community.

use super::*;
use buzz_sdk::sandbox_attach::snapshot::MAX_BLOB_BYTES;
use nostr::hashes::{sha256, Hash};

impl BuzzClient {
    /// Download exactly the approved ciphertext from this community's Blossom server.
    ///
    /// Redirects, oversized or truncated responses, and a mismatched hash are refused.
    /// The complete retry operation has a two-minute deadline. Callers must also
    /// monitor current snapshot authority and cancel this future if it ends.
    /// This method returns ciphertext only; it does not authorize or decrypt it.
    pub(crate) async fn download_snapshot_blob(
        &self,
        hash: &str,
        expected_bytes: u64,
    ) -> Result<Vec<u8>, CliError> {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || !(26..=MAX_BLOB_BYTES as u64).contains(&expected_bytes)
        {
            return Err(CliError::Usage(
                "invalid snapshot hash or byte count".into(),
            ));
        }
        let url = media_url_from_input(&self.relay_url, hash)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        tokio::time::timeout(
            Duration::from_secs(120),
            self.with_retry_body(|| async {
                let auth = sign_blossom_get(&self.keys, &url)?;
                let mut response = self
                    .with_auth_tag(http.get(&url).header("Authorization", auth))
                    .send()
                    .await?;
                if !response.status().is_success() {
                    return Err(CliError::Relay {
                        status: response.status().as_u16(),
                        body: "snapshot blob download refused".into(),
                    });
                }
                if response
                    .content_length()
                    .is_some_and(|n| n != expected_bytes)
                {
                    return Err(CliError::Other(
                        "snapshot byte count does not match receipt".into(),
                    ));
                }
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(expected_bytes as usize)
                    .map_err(|_| {
                        CliError::Other("snapshot download memory budget unavailable".into())
                    })?;
                while let Some(chunk) = response.chunk().await? {
                    if chunk.len() > expected_bytes as usize - bytes.len() {
                        return Err(CliError::Other(
                            "snapshot download exceeds receipt byte count".into(),
                        ));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                if bytes.len() as u64 != expected_bytes
                    || sha256::Hash::hash(&bytes).to_string() != hash
                {
                    return Err(CliError::Other(
                        "snapshot byte count or hash does not match receipt".into(),
                    ));
                }
                Ok(bytes)
            }),
        )
        .await
        .map_err(|_| CliError::Other("snapshot download deadline exceeded".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::HeaderMap, response::Response, routing::get, Router};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use futures_util::{stream, StreamExt};
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    async fn fixture(app: Router) -> (BuzzClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            BuzzClient::new(format!("http://{address}"), Keys::generate(), None, None).unwrap(),
            server,
        )
    }

    fn hash(bytes: &[u8]) -> String {
        sha256::Hash::hash(bytes).to_string()
    }

    #[tokio::test]
    async fn snapshot_get_is_signed_and_exactly_bound_to_hash_and_size() {
        let bytes = vec![b'x'; 64];
        let digest = hash(&bytes);
        let expected = bytes.clone();
        let route = format!("/media/{digest}");
        let app = Router::new().route(
            &route,
            get(move |headers: HeaderMap| {
                let bytes = bytes.clone();
                async move {
                    let encoded = headers["authorization"]
                        .to_str()
                        .unwrap()
                        .strip_prefix("Nostr ")
                        .unwrap();
                    let event: nostr::Event =
                        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
                    event.verify().unwrap();
                    assert_eq!(event.kind.as_u16(), 24242);
                    assert!(event.tags.iter().any(|tag| tag.as_slice() == ["t", "get"]));
                    bytes
                }
            }),
        );
        let (client, server) = fixture(app).await;
        assert_eq!(
            client.download_snapshot_blob(&digest, 64).await.unwrap(),
            expected
        );
        server.abort();
    }

    #[tokio::test]
    async fn snapshot_get_rejects_invalid_input_before_any_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let (client, server) = fixture(Router::new().fallback(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async { "unexpected" }
        }))
        .await;
        for (digest, bytes) in [
            ("https://elsewhere.invalid/file".into(), 64),
            ("a".repeat(64), 0),
            ("A".repeat(64), 64),
            ("a".repeat(64), MAX_BLOB_BYTES as u64 + 1),
        ] {
            assert!(client.download_snapshot_blob(&digest, bytes).await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn chunked_snapshot_size_and_digest_failures_are_refused() {
        for (payload, expected_bytes, digest) in [
            (vec![b'x'; 65], 64, hash(&[b'x'; 64])),
            (vec![b'x'; 63], 64, hash(&[b'x'; 63])),
            (vec![b'y'; 64], 64, hash(&[b'x'; 64])),
        ] {
            let route = format!("/media/{digest}");
            let app = Router::new().route(
                &route,
                get(move || {
                    let payload = payload.clone();
                    async move {
                        Body::from_stream(stream::iter([Ok::<_, Infallible>(bytes::Bytes::from(
                            payload,
                        ))]))
                    }
                }),
            );
            let (client, server) = fixture(app).await;
            assert!(client
                .download_snapshot_blob(&digest, expected_bytes)
                .await
                .is_err());
            server.abort();
        }
    }

    #[tokio::test]
    async fn oversized_chunk_is_refused_before_a_stalled_body_finishes() {
        let digest = hash(&[b'x'; 64]);
        let route = format!("/media/{digest}");
        let (client, server) = fixture(Router::new().route(
            &route,
            get(|| async {
                Body::from_stream(
                    stream::once(async { Ok::<_, Infallible>(bytes::Bytes::from(vec![b'x'; 65])) })
                        .chain(stream::pending()),
                )
            }),
        ))
        .await;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            client.download_snapshot_blob(&digest, 64),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("exceeds receipt byte count"));
        server.abort();
    }

    #[tokio::test]
    async fn oversized_header_is_refused_before_reading_a_stalled_body() {
        let digest = hash(&[b'x'; 64]);
        let route = format!("/media/{digest}");
        let (client, server) = fixture(Router::new().route(
            &route,
            get(|| async {
                Response::builder()
                    .header("content-length", "65")
                    .body(Body::from_stream(
                        stream::once(async {
                            Ok::<_, Infallible>(bytes::Bytes::from_static(b"x"))
                        })
                        .chain(stream::pending()),
                    ))
                    .unwrap()
            }),
        ))
        .await;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            client.download_snapshot_blob(&digest, 64),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("byte count does not match receipt"));
        server.abort();
    }

    #[tokio::test]
    async fn snapshot_redirect_never_forwards_authorization() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let (destination, destination_server) = fixture(Router::new().fallback(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            async { vec![b'x'; 64] }
        }))
        .await;
        let location = destination.relay_url().to_string();
        let digest = hash(&[b'x'; 64]);
        let route = format!("/media/{digest}");
        let (client, server) = fixture(Router::new().route(
            &route,
            get(move || {
                let location = location.clone();
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
        assert!(client.download_snapshot_blob(&digest, 64).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
        destination_server.abort();
    }

    #[tokio::test]
    async fn stalled_snapshot_body_has_a_total_deadline() {
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let digest = hash(&[b'x'; 64]);
        let route = format!("/media/{digest}");
        let (client, server) = fixture(Router::new().route(
            &route,
            get(move || {
                let signal = signal.clone();
                async move {
                    signal.notify_one();
                    Body::from_stream(
                        stream::once(async {
                            Ok::<_, Infallible>(bytes::Bytes::from_static(b"x"))
                        })
                        .chain(stream::pending()),
                    )
                }
            }),
        ))
        .await;
        let task = tokio::spawn(async move { client.download_snapshot_blob(&digest, 64).await });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("deadline exceeded"));
        tokio::time::resume();
        server.abort();
    }
}
