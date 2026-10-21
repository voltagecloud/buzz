//! The avatar editor (`AvatarEditor.tsx`) and the files it uploads
//! (`avatar-upload.ts`): PNG, JPEG, GIF, WebP and HEIC/HEIF up to 50 MiB,
//! HEIC converted to JPEG first. Metadata stripping happens in the relay
//! client's upload, as for message attachments.

mod editor;
mod raster;

pub use editor::{AvatarDismissed, AvatarDone, AvatarEditor, Preview, WIDTH as EDITOR_WIDTH};

use crate::ui::overlay;
use buzz_ui::{Popover, PopoverWidth};
use gpui_kit::*;

use buzz_relay_client::upload::{UploadFile, sniff_image};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_BYTES: u64 = 50 * 1024 * 1024;
const TYPES: &str = "Choose a PNG, JPEG, GIF, WebP or HEIC image.";
const TOO_LARGE: &str = "Choose an image smaller than 50 MiB.";
/// `UPLOAD_FAILURES` in buzz-app's attachments.
const FFMPEG: &str = "Media conversion requires ffmpeg on this computer (version 8 or newer for \
                      HEIC images). Install or update it, then restart the app.";
const HEIC_FAILED: &str = "Image conversion failed. This HEIC/HEIF photo could not be prepared.";
const IO_FAILED: &str = "Media preparation could not access temporary storage. Check available \
                         disk space and retry.";
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(60);

/// The "Edit avatar" popover, 8px under the badge and centred on it. A
/// press anywhere else closes it without a change.
pub fn popover(
    window: &Window,
    editor: Entity<AvatarEditor>,
    on_dismiss: overlay::Dismiss,
) -> impl IntoElement {
    let size = window.viewport_size();
    div()
        .absolute()
        .top_full()
        .left(relative(0.5))
        .child(
            deferred(
                anchored().position(point(px(0.), px(0.))).child(
                    div()
                        .id("avatar-backdrop")
                        .occlude()
                        .w(size.width)
                        .h(size.height)
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            on_dismiss(window, cx)
                        }),
                ),
            )
            .with_priority(2),
        )
        .child(
            deferred(
                anchored()
                    .anchor(Anchor::TopCenter)
                    .snap_to_window_with_margin(px(12.))
                    .child(
                        div().id("avatar-popover").occlude().mt(px(8.)).child(
                            Popover::new()
                                .width(PopoverWidth::Auto)
                                .child(div().w(px(EDITOR_WIDTH - 26.)).child(editor)),
                        ),
                    ),
            )
            .with_priority(3),
        )
}

/// Reads a chosen avatar, refusing anything but a web image or a HEIC
/// photo, which becomes a JPEG.
pub fn read(path: &Path) -> Result<UploadFile, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("Couldn’t read that file: {error}"))?
        .len();
    if size == 0 || size > MAX_BYTES {
        return Err(TOO_LARGE.into());
    }
    let bytes = std::fs::read(path).map_err(|error| format!("Couldn’t read that file: {error}"))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "avatar".into());
    if is_heic(&bytes, &name) {
        let jpeg = convert_heic(path)?;
        let stem = name
            .rsplit_once('.')
            .map_or(name.as_str(), |(stem, _)| stem);
        return Ok(UploadFile {
            name: format!("{stem}.jpg"),
            mime: "image/jpeg".into(),
            bytes: jpeg,
        });
    }
    let mime = sniff_image(&bytes)
        .filter(|mime| {
            matches!(
                *mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        })
        .ok_or(TYPES)?;
    Ok(UploadFile {
        name,
        mime: mime.into(),
        bytes,
    })
}

/// Old Buzz's HEIF brands, or a `.heic`/`.heif` name (`isHeic`).
fn is_heic(bytes: &[u8], name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".heic") || lower.ends_with(".heif") {
        return true;
    }
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    bytes[8..bytes.len().min(32)]
        .as_chunks::<4>()
        .0
        .iter()
        .any(|brand| {
            matches!(
                brand,
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1"
            )
        })
}

/// HEIC to JPEG through ffmpeg 8 or newer, which assembles tile grids
/// (`media_preparation.rs`): fixed arguments, metadata dropped, at most
/// 60 seconds and 50 MiB.
fn convert_heic(source: &Path) -> Result<Vec<u8>, String> {
    let version = ffmpeg()
        .arg("-version")
        .stdout(Stdio::piped())
        .output()
        .map_err(|_| FFMPEG.to_string())?;
    if !version.status.success() || !supports_heic_grids(&version.stdout) {
        return Err(FFMPEG.into());
    }
    let directory = private_dir().map_err(|_| IO_FAILED.to_string())?;
    let output = directory.path.join("prepared.jpg");
    let mut child = ffmpeg()
        .args(["-y", "-nostdin", "-loglevel", "error"])
        .args(["-protocol_whitelist", "file,pipe", "-f", "mov"])
        .args(["-enable_drefs", "0", "-use_absolute_path", "0"])
        .arg("-i")
        .arg(source)
        .args([
            "-map_metadata",
            "-1",
            "-frames:v",
            "1",
            "-fflags",
            "+bitexact",
            "-flags:v",
            "+bitexact",
            "-q:v",
            "2",
        ])
        .arg(&output)
        .stdout(Stdio::null())
        .spawn()
        .map_err(|_| FFMPEG.to_string())?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|_| IO_FAILED.to_string())? {
            break status;
        }
        let oversized = output.metadata().is_ok_and(|meta| meta.len() > MAX_BYTES);
        if oversized || started.elapsed() > CONVERSION_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if oversized { TOO_LARGE } else { HEIC_FAILED }.into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        return Err(HEIC_FAILED.into());
    }
    let mut jpeg = Vec::new();
    std::fs::File::open(&output)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut jpeg))
        .map_err(|_| IO_FAILED.to_string())?;
    if jpeg.is_empty() || jpeg.len() as u64 > MAX_BYTES {
        return Err(TOO_LARGE.into());
    }
    Ok(jpeg)
}

fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    // No console window opens beside the app.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
    command
}

fn supports_heic_grids(version: &[u8]) -> bool {
    std::str::from_utf8(version)
        .ok()
        .and_then(|text| text.strip_prefix("ffmpeg version "))
        .and_then(|text| text.strip_prefix('n').unwrap_or(text).split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 8)
}

/// An owner-only scratch folder, removed when dropped.
struct PrivateDir {
    path: std::path::PathBuf,
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn private_dir() -> std::io::Result<PrivateDir> {
    let path = std::env::temp_dir().join(format!("buzz-avatar-{}", uuid::Uuid::new_v4()));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&path)?;
    Ok(PrivateDir { path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn scratch() -> PrivateDir {
        private_dir().unwrap()
    }

    #[test]
    fn reads_images_and_refuses_other_files() {
        let dir = scratch();
        let png = dir.path.join("a.png");
        std::fs::write(
            &png,
            include_bytes!("../../../../../../assets/icons/buzz-128.png"),
        )
        .unwrap();
        let file = read(&png).unwrap();
        assert_eq!(
            (file.name.as_str(), file.mime.as_str()),
            ("a.png", "image/png")
        );
        let text = dir.path.join("a.txt");
        std::fs::write(&text, b"hello").unwrap();
        assert_eq!(read(&text).unwrap_err(), TYPES);
        let empty = dir.path.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(read(&empty).unwrap_err(), TOO_LARGE);
    }

    #[test]
    fn recognises_heic_like_the_reference() {
        let bmff = |brand: &[u8]| {
            let mut bytes = vec![0, 0, 0, 24];
            bytes.extend_from_slice(b"ftyp");
            bytes.extend_from_slice(brand);
            bytes.extend_from_slice(&[0; 12]);
            bytes
        };
        assert!(is_heic(&bmff(b"heic"), "photo"));
        assert!(is_heic(&bmff(b"mif1"), "photo"));
        assert!(is_heic(&bmff(b"zzzz"), "PHOTO.HEIF"));
        assert!(!is_heic(&bmff(b"isom"), "clip.mp4"));
        assert!(!is_heic(b"\x89PNG\r\n\x1a\n", "a.png"));
        assert!(supports_heic_grids(b"ffmpeg version 8.1.2 Copyright"));
        assert!(supports_heic_grids(b"ffmpeg version n8.0 Copyright"));
        assert!(!supports_heic_grids(b"ffmpeg version 7.1 Copyright"));
    }

    /// A 600×400 gradient from libheif's `heif-enc`. Runs where ffmpeg 8
    /// or newer is installed; otherwise the refusal is checked.
    #[test]
    fn converts_heic_to_jpeg() {
        let dir = scratch();
        let heic = dir.path.join("photo.heic");
        std::fs::write(&heic, include_bytes!("testdata/photo.heic")).unwrap();
        let ready = ffmpeg()
            .arg("-version")
            .stdout(Stdio::piped())
            .output()
            .is_ok_and(|out| out.status.success() && supports_heic_grids(&out.stdout));
        if !ready {
            assert_eq!(read(&heic).unwrap_err(), FFMPEG);
            return;
        }
        let file = read(&heic).unwrap();
        let decoded = image::load_from_memory(&file.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (600, 400));
        assert_eq!(
            (file.name.as_str(), file.mime.as_str()),
            ("photo.jpg", "image/jpeg")
        );
        assert_eq!(sniff_image(&file.bytes), Some("image/jpeg"));
    }
}
