//! buzz-desktop's media services against the local TLS test relay: a
//! prepared image and a converted video upload, then stream with ranged
//! reads from the loopback listener, download without replacing files, and
//! copy as pixels. buzz-desktop serves HTTPS relays only, so these need the
//! TLS relay. Skipped unless `BUZZ_TEST_RELAY_TLS` points at `.test-relay`.
//! A test binary of its own, since it sets `SSL_CERT_FILE` for the test CA.

use buzz_identity::Identity;
use buzz_relay_client::upload::{self, Source, Upload};
use buzz_relay_client::*;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn identities(dir: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
        .unwrap()
}

fn alice(identities: &Value) -> Arc<Identity> {
    Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    )
}

/// A PNG no earlier run uploaded, so the relay stores a new blob.
fn unique_png() -> Vec<u8> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let image = image::RgbaImage::from_fn(12, 9, |x, y| {
        image::Rgba([
            (seed >> (x % 8)) as u8,
            (seed >> 8) as u8 ^ y as u8,
            90,
            255,
        ])
    });
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_downloads_and_copies_over_tls() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY_TLS") else {
        eprintln!("BUZZ_TEST_RELAY_TLS not set; skipping");
        return;
    };
    let tls: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/tls/relay.json")).unwrap())
            .unwrap();
    // Before any client exists: trust the test CA.
    // SAFETY: no other thread of this test binary has started yet.
    unsafe { std::env::set_var("SSL_CERT_FILE", tls["ca_bundle"].as_str().unwrap()) };
    let origin = RelayOrigin::parse(tls["relay_ws_url"].as_str().unwrap()).unwrap();
    let media = Media::new(
        tokio::runtime::Handle::current(),
        origin,
        alice(&identities(&dir)),
    );

    let png = unique_png();
    let image = upload::upload(
        &media,
        upload::UploadFile {
            name: "square.png".into(),
            mime: "image/png".into(),
            bytes: png,
        },
    )
    .await
    .unwrap();
    assert!(
        image.url.starts_with("https://localhost:53443/media/"),
        "{}",
        image.url
    );
    let stored = media.fetch(&image.url).await.unwrap();

    // Players read ranges from the loopback listener.
    let stream = media.stream_url(&image.url).expect("HTTPS media streams");
    assert!(stream.starts_with("http://127.0.0.1:"), "{stream}");
    let client = reqwest::Client::new();
    let part = client
        .get(&stream)
        .header("Range", "bytes=1-4")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status().as_u16(), 206);
    assert_eq!(&part.bytes().await.unwrap()[..], &stored[1..5]);
    assert!(media.stream_url("https://example.com/a.png").is_none());

    // Downloads never replace a file.
    let downloads = tempfile::tempdir().unwrap();
    let first = media
        .download(&image.url, Some("square.png"), downloads.path().to_owned())
        .await
        .unwrap();
    let second = media
        .download(&image.url, Some("square.png"), downloads.path().to_owned())
        .await
        .unwrap();
    assert_eq!(first.file_name().unwrap(), "square.png");
    assert_eq!(second.file_name().unwrap(), "square (1).png");
    assert_eq!(std::fs::read(&first).unwrap(), &stored[..]);

    let (width, height, rgba) = media.copy_image(&image.url).await.unwrap();
    assert_eq!((width, height, rgba.len()), (12, 9, 12 * 9 * 4));

    // A converted video streams as MP4.
    let video = upload::send(
        &media,
        Upload::new(
            "clip.webm",
            "video/webm",
            Source::Path(fixture("clip.webm")),
        ),
        |_| {},
    )
    .await
    .unwrap();
    let head = client
        .get(media.stream_url(&video.url).unwrap())
        .header("Range", "bytes=0-11")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&head[4..8], b"ftyp");
}
