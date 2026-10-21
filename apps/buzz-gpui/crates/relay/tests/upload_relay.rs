//! Uploads through buzz-desktop's upload host against the local test
//! relays: images prepared here, HEIC photos and videos converted with
//! ffmpeg, progress and cancellation, then a message carrying them in a
//! throwaway channel. Skipped unless `BUZZ_TEST_RELAY` points at
//! `.test-relay`. Needs ffmpeg 8 on PATH.

use buzz_identity::Identity;
use buzz_relay_client::lifecycle::NewChannel;
use buzz_relay_client::upload::{self, Source, Stage, Upload, UploadError};
use buzz_relay_client::*;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

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

async fn until<T: Clone>(
    receiver: &mut watch::Receiver<T>,
    what: &str,
    predicate: impl Fn(&T) -> bool,
) -> T {
    tokio::time::timeout(
        Duration::from_secs(30),
        receiver.wait_for(|value| predicate(value)),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    .unwrap()
    .clone()
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

/// Uploads `upload`, recording its stages.
async fn send(media: &Media, upload: Upload) -> (Result<Attachment, UploadError>, Vec<Stage>) {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let record = stages.clone();
    let result = upload::send(media, upload, move |stage| {
        record.lock().unwrap().push(stage)
    })
    .await;
    let stages = stages.lock().unwrap().clone();
    (result, stages)
}

#[tokio::test(flavor = "multi_thread")]
async fn converts_heic_and_video_and_sends_them() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY") else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let identities = identities(&dir);
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        alice(&identities),
        Store::in_memory().unwrap(),
    );
    let mut snapshot = session.snapshot();
    until(&mut snapshot, "the roster", |s| {
        s.roster == RosterStatus::Ready && s.live
    })
    .await;
    let media = session.media();

    // A still image is prepared here and streamed from memory.
    let png = unique_png();
    let (image, stages) = send(
        media,
        Upload::new("square.png", "image/png", Source::Bytes(png.into())),
    )
    .await;
    let image = image.unwrap();
    assert_eq!(image.mime, "image/png");
    assert_eq!(image.name, "square.png");
    assert_eq!(stages.first(), Some(&Stage::Preparing));
    assert!(stages.contains(&Stage::Uploading));
    assert!(
        stages.iter().any(
            |s| matches!(s, Stage::Sent { sent, total } if sent == total && *total == image.size)
        ),
        "{stages:?}"
    );

    // HEIC becomes JPEG, read from disk.
    let (photo, _) = send(
        media,
        Upload::new(
            "photo.heic",
            "image/heic",
            Source::Path(fixture("photo.heic")),
        ),
    )
    .await;
    let photo = photo.unwrap();
    assert_eq!(
        (photo.name.as_str(), photo.mime.as_str()),
        ("photo.jpg", "image/jpeg")
    );

    // WebM and QuickTime become MP4.
    for (name, mime) in [("clip.webm", "video/webm"), ("clip.mov", "video/quicktime")] {
        let (video, stages) =
            send(media, Upload::new(name, mime, Source::Path(fixture(name)))).await;
        let video = video.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            (video.name.as_str(), video.mime.as_str()),
            ("clip.mp4", "video/mp4")
        );
        assert!(stages.contains(&Stage::Uploading), "{stages:?}");
    }

    // A cancel that arrives before the upload starts still stops it.
    let upload = Upload::new(
        "late.bin",
        "application/octet-stream",
        Source::Bytes(vec![7u8; 4096].into()),
    );
    upload::cancel(&upload.id);
    let (cancelled, _) = send(media, upload).await;
    assert_eq!(cancelled, Err(UploadError::Cancelled));

    // A video ffmpeg cannot read is refused before any upload.
    let (refused, _) = send(
        media,
        Upload::new(
            "broken.mp4",
            "video/mp4",
            Source::Bytes(b"not a video".to_vec().into()),
        ),
    )
    .await;
    assert_eq!(refused, Err(UploadError::Video));

    // The relay checks every `imeta` against its stored blob, so an
    // accepted message proves the descriptors.
    let id = session
        .create_channel(NewChannel {
            name: format!("#uploads-{}", std::process::id()),
            description: "Created by a test".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    let view = session.open_channel(&id);
    let content = format!("converted uploads {}", std::process::id());
    session
        .send(&id, &content, Mentions::default(), vec![image, photo], None)
        .await
        .unwrap();
    let mut timeline = view.snapshot.clone();
    until(&mut timeline, "the accepted echo", |t| {
        t.rows.iter().any(|r| {
            r.content.starts_with(&content)
                && r.delivery
                    .as_ref()
                    .is_some_and(|d| d.status == fold::Delivery::Seen)
        })
    })
    .await;
}
