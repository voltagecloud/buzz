//! Authenticated media download against the local test relay. Skipped
//! unless `BUZZ_TEST_RELAY` points at `.test-relay`.

use buzz_identity::{Identity, MediaVerb};
use buzz_relay_client::{Media, MediaError, RelayOrigin};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// A real PNG: the relay decodes uploads and refuses anything else.
const PNG: &[u8] = include_bytes!("../../../assets/icons/buzz-128.png");

#[tokio::test(flavor = "multi_thread")]
async fn downloads_relay_media_with_blossom_auth() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY") else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let server = identities["community_host"].as_str().unwrap();

    let sha = hex(&Sha256::digest(PNG));
    let auth = alice
        .media_auth(MediaVerb::Upload, server, Some(&sha))
        .unwrap();
    let response = reqwest::Client::new()
        .put(origin.endpoint("/upload"))
        .header("Authorization", auth)
        .header("Content-Type", "image/png")
        .header("X-SHA-256", &sha)
        .body(PNG.to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert!(
        status.is_success(),
        "upload: {status} {}",
        response.text().await.unwrap()
    );
    let descriptor: Value = response.json().await.unwrap();
    let url = descriptor["url"].as_str().unwrap().to_string();

    let media = Media::new(tokio::runtime::Handle::current(), origin.clone(), alice);
    assert!(media.is_relay_media(&url), "{url}");
    let bytes = media.fetch(&url).await.unwrap();
    assert_eq!(&bytes[..], PNG);
    // Cached: a second fetch returns the same body without a request.
    assert_eq!(&media.fetch(&url).await.unwrap()[..], PNG);

    // Unsigned requests are refused, which is why the UI fetches here.
    let plain = reqwest::get(&url).await.unwrap();
    assert!(!plain.status().is_success());

    assert_eq!(
        media.fetch("http://example.com/a.png").await,
        Err(MediaError::Unsupported)
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
