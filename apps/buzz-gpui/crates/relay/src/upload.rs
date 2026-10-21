//! Media upload (`docs/port/relay.md` §5n.1-5n.3).
//!
//! Images are prepared here first, as buzz-app's webview does
//! (`prepare-attachment.ts`): metadata the relay may refuse is dropped, and
//! still images are re-rendered with their orientation and colour profile
//! applied. Then the file goes through buzz-desktop's upload host, the code
//! buzz-app's Tauri host runs: it is spooled to disk, HEIC photos and video
//! are converted with ffmpeg when asked to (`video-preparation.ts` picks the
//! mode from the container bytes, as here), and the result is streamed to
//! `PUT /upload` under a Blossom `upload` authorization. The relay's BUD-02
//! descriptor is checked against the community origin before it can become
//! an `imeta` tag.

use crate::media::Media;
use crate::messages::{Attachment, media_byte_limit, validate_upload};
use buzz_desktop::upload::{Spools, UPLOAD_CHUNK, Uploads, process_spool};
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use url::Url;

const MIB: u64 = 1024 * 1024;
/// `UPLOAD_MAX_BYTES`.
pub const MAX_UPLOAD: u64 = 500 * MIB;
/// Larger images are refused rather than decoded.
const MAX_PIXELS: u64 = 25_000_000;
/// Voice notes picked as files are converted only up to this size.
const MAX_VOICE: u64 = 128 * MIB;
/// Container recognition reads this much of a file.
const HEADER: usize = 4096;

/// `UPLOAD_FAILURES`, with the reference's wording.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    #[error("Uploads are unavailable on this connection.")]
    Unavailable,
    #[error(
        "File exceeds the supported limit: images 50 MiB, GIFs 10 MiB, documents 100 MiB, videos 500 MiB. The relay may enforce a lower limit."
    )]
    Size,
    #[error("Image conversion failed. This HEIC/HEIF photo could not be prepared.")]
    Image,
    #[error(
        "Media conversion requires ffmpeg on this computer (version 8 or newer for HEIC images). Install or update it, then restart the app."
    )]
    Ffmpeg,
    #[error(
        "Media preparation could not access temporary storage. Check available disk space and retry."
    )]
    Io,
    #[error("Video preparation failed. This recording could not be converted to MP4.")]
    Video,
    #[error("Uploads are busy. Retry this file shortly.")]
    Capacity,
    #[error(
        "This file needs metadata cleanup before it can be uploaded. Choose an exported copy without metadata, or remove it for now."
    )]
    Metadata,
    #[error("The server could not accept this file. Its format or metadata may not be supported.")]
    Rejected,
    #[error("You don’t have permission to upload here.")]
    Denied,
    #[error("Upload did not finish. Retry or remove this file.")]
    Failed,
    #[error("The server returned an invalid upload result. Retry this file.")]
    Invalid,
    #[error("Upload cancelled. Retry to upload this file.")]
    Cancelled,
    /// An image that could not be prepared, in `prepareAttachment`'s words.
    #[error("{0}")]
    Prepare(&'static str),
}

const PIXELS: UploadError = UploadError::Prepare("Choose an image with at most 25 million pixels.");
const MALFORMED: UploadError = UploadError::Prepare("Image container is malformed.");
const PREPARE_FAILED: UploadError = UploadError::Prepare("Image preparation failed.");

/// A file as the user chose it, already in memory.
#[derive(Clone, Debug)]
pub struct UploadFile {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Where an upload's bytes come from. Files stay on disk until they are
/// spooled; nothing reads a whole video into memory.
#[derive(Clone, Debug)]
pub enum Source {
    Path(PathBuf),
    Bytes(Arc<[u8]>),
}

impl Source {
    pub fn len(&self) -> std::io::Result<u64> {
        match self {
            Source::Path(path) => Ok(std::fs::metadata(path)?.len()),
            Source::Bytes(bytes) => Ok(bytes.len() as u64),
        }
    }

    pub fn is_empty(&self) -> std::io::Result<bool> {
        Ok(self.len()? == 0)
    }

    fn reader(&self) -> std::io::Result<Box<dyn Read + Send>> {
        Ok(match self {
            Source::Path(path) => Box::new(std::fs::File::open(path)?),
            Source::Bytes(bytes) => Box::new(Cursor::new(bytes.clone())),
        })
    }

    /// The first `limit` bytes, for container and image recognition.
    pub fn header(&self, limit: usize) -> std::io::Result<Vec<u8>> {
        let mut header = Vec::with_capacity(limit);
        self.reader()?.take(limit as u64).read_to_end(&mut header)?;
        Ok(header)
    }

    fn read_all(&self) -> std::io::Result<Vec<u8>> {
        match self {
            Source::Path(path) => std::fs::read(path),
            Source::Bytes(bytes) => Ok(bytes.to_vec()),
        }
    }
}

/// One upload: its ID (for [`cancel`]), the file's name and type hint, and
/// its bytes.
#[derive(Clone, Debug)]
pub struct Upload {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub source: Source,
}

impl Upload {
    pub fn new(name: impl Into<String>, mime: impl Into<String>, source: Source) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            mime: mime.into(),
            source,
        }
    }
}

/// Where an upload is (`DraftAttachment.status` and `transfer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Image metadata cleanup and spooling.
    Preparing,
    /// Handed to the upload host: converting, or sending with no byte
    /// counts yet.
    Uploading,
    /// Bytes handed to the connection, of `total`.
    Sent { sent: u64, total: u64 },
}

/// The upload host: received spools and the uploads that can be cancelled.
/// One per process, like the Tauri host's managed state.
struct Host {
    spools: Spools,
    uploads: Uploads,
}

static SPOOL_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

/// Sets where upload spools live, such as the app's cache directory. Call
/// it before the first upload; otherwise they go under the temp directory.
pub fn set_spool_directory(path: PathBuf) {
    let _ = SPOOL_DIRECTORY.set(path);
}

fn host() -> &'static Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    HOST.get_or_init(|| {
        let parent = SPOOL_DIRECTORY
            .get()
            .cloned()
            .unwrap_or_else(|| std::env::temp_dir().join("buzz-gpui-upload-spools"));
        Host {
            spools: Spools::new(Ok(parent)),
            uploads: Uploads::default(),
        }
    })
}

/// Stops upload `id` wherever it is, even before it starts. It then fails
/// with [`UploadError::Cancelled`].
pub fn cancel(id: &str) {
    let host = host();
    host.spools.cancel(&host.uploads, id);
}

/// Prepares and uploads one file in memory to the community, returning
/// what a message needs for its `imeta` tag.
pub async fn upload(media: &Media, file: UploadFile) -> Result<Attachment, UploadError> {
    let upload = Upload::new(file.name, file.mime, Source::Bytes(file.bytes.into()));
    send(media, upload, |_| {}).await
}

/// Prepares, converts when needed, and uploads `upload`, reporting each
/// [`Stage`]. Runs on the session's runtime.
pub async fn send(
    media: &Media,
    upload: Upload,
    progress: impl Fn(Stage) + Send + Sync + 'static,
) -> Result<Attachment, UploadError> {
    let task = media.clone();
    media
        .runtime()
        .spawn(async move { send_now(&task, upload, Arc::new(progress)).await })
        .await
        .map_err(|_| UploadError::Failed)?
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, UploadError> + Send + 'static,
) -> Result<T, UploadError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| UploadError::Failed)?
}

/// What preparation ffmpeg must do, from the container bytes
/// (`nativeAttachmentUpload`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Conversion {
    Heic,
    Video(&'static str),
    Voice(&'static str),
}

impl Conversion {
    fn mode(self) -> String {
        match self {
            Conversion::Heic => "image:mov".into(),
            Conversion::Video(demuxer) => format!("video:{demuxer}"),
            Conversion::Voice(demuxer) => format!("voice:{demuxer}"),
        }
    }

    fn output(self) -> (&'static str, &'static str) {
        match self {
            Conversion::Heic => ("image/jpeg", "jpg"),
            _ => ("video/mp4", "mp4"),
        }
    }
}

/// The conversion a file needs, `Ok(None)` for none, or the refusal for a
/// video or voice note ffmpeg cannot read.
fn conversion(name: &str, mime: &str, header: &[u8]) -> Result<Option<Conversion>, UploadError> {
    let voice = is_voice_note(name);
    let found = if voice {
        audio_demuxer(header).map(Conversion::Voice)
    } else if is_heic(header, name) {
        Some(Conversion::Heic)
    } else {
        video_demuxer(header).map(Conversion::Video)
    };
    if found.is_none() && (voice || mime.starts_with("video/")) {
        return Err(UploadError::Video);
    }
    Ok(found)
}

async fn send_now(
    media: &Media,
    upload: Upload,
    progress: Arc<dyn Fn(Stage) + Send + Sync>,
) -> Result<Attachment, UploadError> {
    let Upload {
        id,
        name,
        mime,
        source,
    } = upload;
    let probe = source.clone();
    let (size, header) = blocking(move || {
        let size = probe.len().map_err(|_| UploadError::Failed)?;
        let header = probe.header(HEADER).map_err(|_| UploadError::Failed)?;
        Ok((size, header))
    })
    .await?;
    if size == 0 || size > MAX_UPLOAD {
        return Err(UploadError::Size);
    }
    let converting = conversion(&name, &mime, &header)?;
    if matches!(converting, Some(Conversion::Voice(_))) && size > MAX_VOICE {
        return Err(UploadError::Size);
    }
    progress(Stage::Preparing);
    let (name, kind, source, size) = match (converting, sniff_image(&header)) {
        (None, Some(_)) => {
            let original = source.clone();
            let file_name = name.clone();
            let prepared = blocking(move || {
                let bytes = original.read_all().map_err(|_| UploadError::Failed)?;
                prepare_image(&bytes, &file_name)
            })
            .await?;
            let size = prepared.bytes.len() as u64;
            (
                prepared.name,
                prepared.mime.to_string(),
                Source::Bytes(prepared.bytes.into()),
                size,
            )
        }
        (None, None) => {
            let kind = mime_type(&name, &header);
            if size > media_byte_limit(&kind) {
                return Err(UploadError::Size);
            }
            (name, kind, source, size)
        }
        (Some(_), _) => {
            let kind = mime_type(&name, &header);
            (name, kind, source, size)
        }
    };
    let host = host();
    let spool_id = id.clone();
    blocking(move || spool(host, &spool_id, &source, size)).await?;
    let base = Url::parse(&media.origin().http()).map_err(|_| UploadError::Failed)?;
    let endpoint = base.join("/upload").map_err(|_| UploadError::Failed)?;
    let result = async {
        let (spool, mut cancelled) = host.spools.take(&id).map_err(|error| host_error(&error))?;
        progress(Stage::Uploading);
        let report = progress.clone();
        let mode = converting.map(Conversion::mode);
        process_spool(
            &media.signer(),
            endpoint,
            spool,
            Some(&kind),
            mode.as_deref(),
            move |sent| {
                report(Stage::Sent {
                    sent: sent.sent,
                    total: sent.total,
                })
            },
            &mut cancelled,
        )
        .await
        .map_err(|error| host_error(&error))
    }
    .await;
    host.spools.discard(&id);
    host.uploads.finish(&id);
    let response = result?;
    let descriptor = read_upload_response(response.status, &response.body)?;
    let (name, size, expected) = match converting {
        Some(conversion) => {
            let (kind, extension) = conversion.output();
            let stem = name
                .rsplit_once('.')
                .map_or(name.as_str(), |(stem, _)| stem);
            let stem = if stem.is_empty() { "Attachment" } else { stem };
            (format!("{stem}.{extension}"), descriptor.size, Some(kind))
        }
        None => (name, size, None),
    };
    if descriptor.size != size || expected.is_some_and(|kind| descriptor.mime != kind) {
        return Err(UploadError::Invalid);
    }
    let mut attachment = Attachment {
        name,
        url: descriptor.url,
        mime: descriptor.mime,
        size: descriptor.size,
        sha256: descriptor.sha256,
        voice_duration: None,
    };
    let (url, mime) =
        validate_upload(&attachment, media.origin()).map_err(|_| UploadError::Invalid)?;
    attachment.url = url;
    attachment.mime = mime;
    Ok(attachment)
}

/// Copies `source` into a new spool in bounded chunks, as the webview's
/// IPC chunks do. Any failure discards it.
fn spool(host: &Host, id: &str, source: &Source, size: u64) -> Result<(), UploadError> {
    let size = usize::try_from(size).map_err(|_| UploadError::Size)?;
    host.spools
        .begin(&host.uploads, id, size)
        .map_err(|error| host_error(&error))?;
    let written = (|| {
        let mut reader = source.reader().map_err(|_| UploadError::Failed)?;
        let mut buffer = vec![0; UPLOAD_CHUNK];
        let mut offset = 0;
        while offset < size {
            let want = UPLOAD_CHUNK.min(size - offset);
            reader
                .read_exact(&mut buffer[..want])
                .map_err(|_| UploadError::Failed)?;
            host.spools
                .append(id, offset, &buffer[..want])
                .map_err(|error| host_error(&error))?;
            offset += want;
        }
        Ok(())
    })();
    if written.is_err() {
        host.spools.cancel(&host.uploads, id);
    }
    written
}

/// The upload host's errors, in `UPLOAD_FAILURES` terms (`nativeUpload`).
fn host_error(error: &str) -> UploadError {
    match error {
        "Upload cancelled" => UploadError::Cancelled,
        "Uploads are busy" => UploadError::Capacity,
        "File exceeds the supported upload limit" => UploadError::Size,
        "Relay response is too large" => UploadError::Invalid,
        _ if error.contains("temporary storage") => UploadError::Io,
        _ => UploadError::Failed,
    }
}

#[derive(serde::Deserialize)]
struct Descriptor {
    url: String,
    sha256: String,
    size: u64,
    #[serde(rename = "type")]
    mime: String,
}

/// `readUploadResponse`: the status and body policy for every upload.
fn read_upload_response(status: u16, body: &str) -> Result<Descriptor, UploadError> {
    match status {
        401 | 403 => return Err(UploadError::Denied),
        413 => return Err(UploadError::Size),
        429 => return Err(UploadError::Capacity),
        _ => {}
    }
    if body.len() > 8192 {
        return Err(UploadError::Invalid);
    }
    if !(200..300).contains(&status) {
        return Err(failure(status, body));
    }
    serde_json::from_str(body).map_err(|_| UploadError::Invalid)
}

/// A refusal's code from its JSON body, else by status.
fn failure(status: u16, body: &str) -> UploadError {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("code")?.as_str().map(str::to_owned));
    match code.as_deref() {
        Some("unavailable") => UploadError::Unavailable,
        Some("size") => UploadError::Size,
        Some("image") => UploadError::Image,
        Some("ffmpeg") => UploadError::Ffmpeg,
        Some("io") => UploadError::Io,
        Some("video") => UploadError::Video,
        Some("capacity") => UploadError::Capacity,
        Some("metadata") => UploadError::Metadata,
        Some("rejected") => UploadError::Rejected,
        Some("denied") => UploadError::Denied,
        Some("failed") => UploadError::Failed,
        Some("invalid") => UploadError::Invalid,
        Some("cancelled") => UploadError::Cancelled,
        _ if matches!(status, 400 | 415 | 422) => {
            if body.to_ascii_lowercase().contains("metadata") {
                UploadError::Metadata
            } else {
                UploadError::Rejected
            }
        }
        _ => UploadError::Failed,
    }
}

// ---------------------------------------------------------------------------
// Container recognition (`video-preparation.ts`, `attachment-limits.ts`)

/// The image type from the first bytes (`imageType`).
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The type to declare: sniffed for images, else from the extension. The
/// relay sniffs again and ignores what the client says.
pub fn mime_type(name: &str, bytes: &[u8]) -> String {
    if let Some(image) = sniff_image(bytes) {
        return image.into();
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
    .into()
}

fn ascii(bytes: &[u8], range: std::ops::Range<usize>) -> &[u8] {
    bytes.get(range).unwrap_or_default()
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// `videoDemuxer`: the container's fixed ffmpeg demuxer, never anything
/// the caller supplies.
pub fn video_demuxer(bytes: &[u8]) -> Option<&'static str> {
    let text = |a, b| ascii(bytes, a..b);
    if text(0, 4) == b"RIFF" && text(8, 12) == b"AVI " {
        return Some("avi");
    }
    if text(0, 4) == b"\x1a\x45\xdf\xa3" {
        return Some("matroska");
    }
    if text(0, 3) == b"FLV" {
        return Some("flv");
    }
    if text(0, 10) == b"\x30\x26\xb2\x75\x8e\x66\xcf\x11\xa6\xd9" {
        return Some("asf");
    }
    if text(0, 3) == b"\x00\x00\x01"
        && let Some(&code) = bytes.get(3)
    {
        if (0xba..=0xbf).contains(&code) {
            return Some("mpeg");
        }
        if matches!(code, 0xb0 | 0xb1 | 0xb5 | 0xb6) {
            return Some("m4v");
        }
        if (0xb2..=0xb9).contains(&code) {
            return Some("mpegvideo");
        }
    }
    const STILLS: [&[u8]; 10] = [
        b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1", b"avif", b"avis",
    ];
    let mut offset = 0;
    while offset + 12 <= bytes.len() {
        let kind = text(offset + 4, offset + 8);
        if kind == b"ftyp" {
            // HEIF and AVIF stills are not videos, despite the container.
            let end = bytes.len().min(offset + be32(bytes, offset)? as usize);
            let mut brand = offset + 8;
            while brand + 4 <= end {
                if STILLS.contains(&text(brand, brand + 4)) {
                    return None;
                }
                brand += if brand == offset + 8 { 8 } else { 4 };
            }
            let major = text(offset + 8, offset + 12);
            return video_brand(major).then_some("mov");
        }
        if kind == b"moov" || kind == b"mdat" {
            return Some("mov");
        }
        if !matches!(kind, b"wide" | b"free" | b"skip") {
            break;
        }
        let size = be32(bytes, offset)? as usize;
        if size < 8 {
            break;
        }
        offset += size;
    }
    None
}

/// The video brands old Buzz recognized; M4A audio and unknown brands are
/// not videos.
fn video_brand(brand: &[u8]) -> bool {
    let Ok(brand) = <[u8; 4]>::try_from(brand) else {
        return false;
    };
    matches!(
        &brand,
        b"qt  "
            | b"avc1"
            | b"dash"
            | b"mmp4"
            | b"mp41"
            | b"mp42"
            | b"mp4v"
            | b"mp71"
            | b"MSNV"
            | b"NDAS"
            | b"NSDC"
            | b"F4V "
            | b"F4P "
    ) || (brand.starts_with(b"M4V"))
        || (brand.starts_with(b"iso") && matches!(brand[3], b'2'..=b'6' | b'm'))
        || (brand.starts_with(b"NDS") && b"CHMPS".contains(&brand[3]))
        || (brand.starts_with(b"NDX") && b"CHMPS".contains(&brand[3]))
}

/// `isHeic`: old Buzz's HEIF brands, or a `.heic`/`.heif` name.
pub fn is_heic(bytes: &[u8], name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".heic") || lower.ends_with(".heif") {
        return true;
    }
    if ascii(bytes, 4..8) != b"ftyp" {
        return false;
    }
    let mut offset = 8;
    while offset + 4 <= bytes.len().min(32) {
        if matches!(
            ascii(bytes, offset..offset + 4),
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1"
        ) {
            return true;
        }
        offset += 4;
    }
    false
}

/// `isVoiceNote`: a recording old Buzz saved as a file.
pub fn is_voice_note(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("voice-note-") && lower.ends_with(".wav")
}

/// `audioDemuxer`: only for the voice-note filename exception.
pub fn audio_demuxer(bytes: &[u8]) -> Option<&'static str> {
    let text = |a, b| ascii(bytes, a..b);
    let (first, second) = (bytes.first().copied(), bytes.get(1).copied().unwrap_or(0));
    if text(0, 4) == b"RIFF" && text(8, 12) == b"WAVE" {
        Some("wav")
    } else if text(0, 4) == b"fLaC" {
        Some("flac")
    } else if text(0, 4) == b"OggS" {
        Some("ogg")
    } else if text(0, 4) == b"FORM" && matches!(text(8, 12), b"AIFF" | b"AIFC") {
        Some("aiff")
    } else if text(0, 3) == b"ID3"
        || (first == Some(0xff) && second & 0xe0 == 0xe0 && second & 6 != 0)
    {
        Some("mp3")
    } else if first == Some(0xff) && second & 0xf6 == 0xf0 {
        Some("aac")
    } else if text(4, 8) == b"ftyp" && matches!(text(8, 12), b"M4A " | b"M4B " | b"F4A " | b"F4B ")
    {
        Some("mov")
    } else if text(0, 5) == b"#!AMR" {
        Some("amr")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Image preparation (`prepare-attachment.ts`, `image-metadata.ts`)

/// A prepared image.
pub struct Prepared {
    pub name: String,
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

/// Strips metadata the relay may refuse (EXIF, ICC, text, comments).
/// Animated GIF, PNG and WebP keep every frame byte and lose only their
/// metadata. Still PNG and JPEG, and WebP whose profile or orientation
/// matters, are re-rendered with both applied: JPEG at quality 95, WebP
/// losslessly. An agent snapshot's `tEXt` chunk survives in PNG.
pub fn prepare_image(bytes: &[u8], name: &str) -> Result<Prepared, UploadError> {
    let Some(kind) = sniff_image(bytes) else {
        return Err(PREPARE_FAILED);
    };
    let output = match kind {
        "image/gif" => clean_gif(bytes)?,
        "image/png" => {
            let chunks = png_chunks(bytes)?;
            let snapshot = snapshot_chunk(&chunks).map(<[u8]>::to_vec);
            if chunks.iter().any(|c| c.kind == *b"acTL") {
                clean_png(bytes, None, true)?
            } else {
                let rendered = render(bytes, image::ImageFormat::Png)?;
                clean_png(&rendered, snapshot.as_deref(), false)?
            }
        }
        "image/webp" => {
            let chunks = webp_chunks(bytes)?;
            let animated = chunks.iter().any(|c| matches!(&c.kind, b"ANIM" | b"ANMF"));
            if animated || !webp_needs_transform(&chunks) {
                clean_webp(bytes, animated)?
            } else {
                let rendered = render(bytes, image::ImageFormat::WebP)?;
                clean_webp(&rendered, false)?
            }
        }
        _ => render(bytes, image::ImageFormat::Jpeg)?,
    };
    if output.is_empty() || output.len() as u64 > media_byte_limit(kind) {
        return Err(UploadError::Size);
    }
    Ok(Prepared {
        name: name.to_string(),
        mime: kind,
        bytes: output,
    })
}

/// Decodes `bytes` with its orientation and colour profile applied, as a
/// canvas draws it, and encodes it again as `format`.
fn render(bytes: &[u8], format: image::ImageFormat) -> Result<Vec<u8>, UploadError> {
    use image::{DynamicImage, ImageDecoder, ImageReader};
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader.set_format(sniffed_format(bytes).ok_or(PREPARE_FAILED)?);
    let mut decoder = reader.into_decoder().map_err(|_| PREPARE_FAILED)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(PIXELS);
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let profile = decoder.icc_profile().ok().flatten();
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| PREPARE_FAILED)?;
    image.apply_orientation(orientation);
    let mut rgba = image.into_rgba8();
    if let Some(profile) = profile {
        to_srgb(&profile, &mut rgba);
    }
    let mut out = Vec::new();
    match format {
        image::ImageFormat::Jpeg => {
            // A canvas exports JPEG over black, without alpha.
            let rgb = DynamicImage::ImageRgba8(rgba).into_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
                .encode_image(&rgb)
                .map_err(|_| PREPARE_FAILED)?;
            out = clean_encoded_jpeg(&out)?;
        }
        image::ImageFormat::WebP => {
            image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .encode(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|_| PREPARE_FAILED)?;
        }
        _ => {
            DynamicImage::ImageRgba8(rgba)
                .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
                .map_err(|_| PREPARE_FAILED)?;
        }
    }
    Ok(out)
}

/// Strips encoder-added metadata from a JPEG this client just encoded
/// (`cleanEncodedJpeg`): every APP and COM section except the canonical
/// JFIF and Adobe ones the relay accepts, and anything after the end
/// marker. Only for output whose pixels were already oriented and drawn
/// in sRGB: on a source photo it would lose appearance. Entropy-coded
/// data, stuffed bytes and restart markers stay as they are.
fn clean_encoded_jpeg(bytes: &[u8]) -> Result<Vec<u8>, UploadError> {
    let invalid = || MALFORMED;
    if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return Err(invalid());
    }
    let mut out = bytes[..2].to_vec();
    let mut offset = 2;
    let mut scan = false;
    while offset < bytes.len() {
        let start = offset;
        if scan {
            while offset < bytes.len() {
                if bytes[offset] != 0xff {
                    offset += 1;
                    continue;
                }
                match bytes.get(offset + 1) {
                    Some(0) | Some(0xd0..=0xd7) => offset += 2,
                    _ => break,
                }
            }
            out.extend_from_slice(&bytes[start..offset.min(bytes.len())]);
            offset = offset.min(bytes.len());
            scan = false;
            continue;
        }
        if bytes[offset] != 0xff {
            return Err(invalid());
        }
        offset += 1;
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let Some(&marker) = bytes.get(offset) else {
            return Err(invalid());
        };
        offset += 1;
        if marker == 0xd9 {
            out.extend_from_slice(&[0xff, 0xd9]);
            return Ok(out);
        }
        if marker == 0 || marker == 0xd8 {
            return Err(invalid());
        }
        if marker == 1 || (0xd0..=0xd7).contains(&marker) {
            out.extend_from_slice(&bytes[start..offset]);
            continue;
        }
        if offset + 2 > bytes.len() {
            return Err(invalid());
        }
        let length = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        let end = offset + length;
        if length < 2 || end > bytes.len() {
            return Err(invalid());
        }
        let payload = &bytes[offset + 2..end];
        // The relay's canonical JFIF and Adobe exceptions; every other APP
        // or COM section is metadata.
        let canonical = (marker == 0xe0
            && payload.len() >= 14
            && payload.starts_with(b"JFIF\0")
            && payload.len() == 14 + 3 * payload[12] as usize * payload[13] as usize)
            || (marker == 0xee && payload.len() == 12 && payload.starts_with(b"Adobe"));
        if canonical || !((0xe0..=0xef).contains(&marker) || marker == 0xfe) {
            out.extend_from_slice(&bytes[start..end]);
        }
        offset = end;
        scan = marker == 0xda;
    }
    Err(invalid())
}

fn sniffed_format(bytes: &[u8]) -> Option<image::ImageFormat> {
    Some(match sniff_image(bytes)? {
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/png" => image::ImageFormat::Png,
        "image/gif" => image::ImageFormat::Gif,
        _ => image::ImageFormat::WebP,
    })
}

/// Converts pixels in the embedded profile's colours to sRGB. A profile
/// that cannot be read leaves them as they are, as browsers do.
fn to_srgb(profile: &[u8], rgba: &mut image::RgbaImage) {
    use moxcms::{ColorProfile, Layout, TransformOptions};
    let Ok(source) = ColorProfile::new_from_slice(profile) else {
        return;
    };
    let Ok(transform) = source.create_transform_8bit(
        Layout::Rgba,
        &ColorProfile::new_srgb(),
        Layout::Rgba,
        TransformOptions::default(),
    ) else {
        return;
    };
    let mut converted = vec![0; rgba.as_raw().len()];
    if transform.transform(rgba.as_raw(), &mut converted).is_ok() {
        rgba.copy_from_slice(&converted);
    }
}

struct Chunk<'a> {
    kind: [u8; 4],
    payload: &'a [u8],
    raw: &'a [u8],
}

fn png_chunks(bytes: &[u8]) -> Result<Vec<Chunk<'_>>, UploadError> {
    let mut chunks = Vec::new();
    let mut offset = 8;
    while offset + 12 <= bytes.len() {
        let length = be32(bytes, offset).ok_or(MALFORMED)? as usize;
        let end = offset
            .checked_add(length)
            .and_then(|end| end.checked_add(12))
            .filter(|end| *end <= bytes.len())
            .ok_or(MALFORMED)?;
        let kind: [u8; 4] = bytes[offset + 4..offset + 8]
            .try_into()
            .expect("four bytes");
        chunks.push(Chunk {
            kind,
            payload: &bytes[offset + 8..end - 4],
            raw: &bytes[offset..end],
        });
        if &kind == b"IEND" {
            return Ok(chunks);
        }
        offset = end;
    }
    Err(MALFORMED)
}

/// An agent or team snapshot riding in a PNG's `tEXt` chunk.
fn snapshot_chunk<'a>(chunks: &[Chunk<'a>]) -> Option<&'a [u8]> {
    chunks
        .iter()
        .find(|chunk| {
            &chunk.kind == b"tEXt"
                && [&b"buzz_agent_snapshot\0"[..], b"buzz_team_snapshot\0"]
                    .iter()
                    .any(|key| chunk.payload.starts_with(key))
        })
        .map(|chunk| chunk.raw)
}

/// Whether EXIF data asks for an orientation other than the default.
fn oriented(exif: &[u8]) -> bool {
    let b = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif);
    let little = match b.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return false,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let bytes: [u8; 2] = b.get(at..at + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    if b.len() < 8 || u16_at(2) != Some(42) {
        return false;
    }
    let Some(offset) = u32_at(4).map(|o| o as usize) else {
        return false;
    };
    let Some(count) = u16_at(offset) else {
        return false;
    };
    for i in 0..count as usize {
        let p = offset + 2 + i * 12;
        if p + 12 > b.len() {
            return false;
        }
        if u16_at(p) == Some(0x112) && u16_at(p + 2) == Some(3) && u32_at(p + 4) == Some(1) {
            return u16_at(p + 8).is_some_and(|o| (2..=8).contains(&o));
        }
    }
    false
}

/// Animation frames cannot be re-rendered, so a profile or orientation
/// that would change how they look is refused.
fn assert_animation_appearance(
    chunks: &[Chunk<'_>],
    icc: &[u8; 4],
    exif: &[u8; 4],
) -> Result<(), UploadError> {
    if chunks.iter().any(|c| &c.kind == icc) {
        return Err(UploadError::Prepare(
            "Animated images with an ICC profile cannot be cleaned without changing their colors.",
        ));
    }
    if chunks
        .iter()
        .any(|c| &c.kind == exif && oriented(c.payload))
    {
        return Err(UploadError::Prepare(
            "Animated images with EXIF orientation cannot be cleaned without changing their appearance.",
        ));
    }
    Ok(())
}

/// Critical chunks and the ancillary ones that affect rendering or
/// animation (`isPngRenderingChunk`).
fn png_rendering(kind: &[u8; 4]) -> bool {
    kind[0] & 32 == 0
        || matches!(
            kind,
            b"cHRM"
                | b"gAMA"
                | b"sBIT"
                | b"sRGB"
                | b"bKGD"
                | b"hIST"
                | b"tRNS"
                | b"sPLT"
                | b"acTL"
                | b"fcTL"
                | b"fdAT"
        )
}

fn clean_png(
    bytes: &[u8],
    snapshot: Option<&[u8]>,
    animated: bool,
) -> Result<Vec<u8>, UploadError> {
    let chunks = png_chunks(bytes)?;
    if chunks.first().map(|c| &c.kind) != Some(b"IHDR") {
        return Err(MALFORMED);
    }
    if animated {
        assert_animation_appearance(&chunks, b"iCCP", b"eXIf")?;
    }
    let mut out = bytes[..8].to_vec();
    for chunk in &chunks {
        if png_rendering(&chunk.kind) {
            out.extend_from_slice(chunk.raw);
        }
        if &chunk.kind == b"IHDR"
            && let Some(snapshot) = snapshot
        {
            out.extend_from_slice(snapshot);
        }
    }
    Ok(out)
}

fn riff_chunks(bytes: &[u8], start: usize, end: usize) -> Result<Vec<Chunk<'_>>, UploadError> {
    let mut chunks = Vec::new();
    let mut offset = start;
    while offset < end {
        if offset + 8 > end {
            return Err(MALFORMED);
        }
        let length = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        let next = offset + 8 + length + (length & 1);
        if next > end {
            return Err(MALFORMED);
        }
        chunks.push(Chunk {
            kind: bytes[offset..offset + 4].try_into().expect("four bytes"),
            payload: &bytes[offset + 8..offset + 8 + length],
            raw: &bytes[offset..next],
        });
        offset = next;
    }
    Ok(chunks)
}

fn webp_chunks(bytes: &[u8]) -> Result<Vec<Chunk<'_>>, UploadError> {
    if bytes.len() < 12 {
        return Err(MALFORMED);
    }
    let end = u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes")) as usize + 8;
    if end < 12 || end > bytes.len() {
        return Err(MALFORMED);
    }
    riff_chunks(bytes, 12, end)
}

fn webp_needs_transform(chunks: &[Chunk<'_>]) -> bool {
    chunks
        .iter()
        .any(|c| &c.kind == b"ICCP" || (&c.kind == b"EXIF" && oriented(c.payload)))
}

fn riff_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = kind.to_vec();
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// Keeps the image, alpha and animation chunks and clears the metadata
/// flags of `VP8X`.
fn clean_webp(bytes: &[u8], animated: bool) -> Result<Vec<u8>, UploadError> {
    let chunks = webp_chunks(bytes)?;
    if animated {
        assert_animation_appearance(&chunks, b"ICCP", b"EXIF")?;
    }
    let mut parts = Vec::new();
    for chunk in &chunks {
        if !matches!(
            &chunk.kind,
            b"VP8 " | b"VP8L" | b"VP8X" | b"ALPH" | b"ANIM" | b"ANMF"
        ) {
            continue;
        }
        let mut clean = chunk.payload.to_vec();
        if &chunk.kind == b"VP8X" {
            let flags = clean.first_mut().ok_or(MALFORMED)?;
            *flags &= !0x2c;
        }
        if &chunk.kind == b"ANMF" {
            if chunk.payload.len() < 16 {
                return Err(MALFORMED);
            }
            let mut frame = chunk.payload[..16].to_vec();
            let (mut alpha, mut image) = (false, false);
            for child in riff_chunks(chunk.payload, 16, chunk.payload.len())? {
                match &child.kind {
                    b"ALPH" => {
                        if alpha || image {
                            return Err(MALFORMED);
                        }
                        alpha = true;
                    }
                    b"VP8 " | b"VP8L" => {
                        if image || (alpha && &child.kind == b"VP8L") {
                            return Err(MALFORMED);
                        }
                        image = true;
                    }
                    _ => continue,
                }
                frame.extend(riff_chunk(&child.kind, child.payload));
            }
            if !image {
                return Err(MALFORMED);
            }
            clean = frame;
        }
        parts.push(riff_chunk(&chunk.kind, &clean));
    }
    let body: usize = parts.iter().map(Vec::len).sum();
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&((4 + body) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    for part in parts {
        out.extend(part);
    }
    Ok(out)
}

/// Copies the image data and the graphic-control and looping extensions;
/// drops comments, plain text and other application extensions.
fn clean_gif(b: &[u8]) -> Result<Vec<u8>, UploadError> {
    if b.len() < 13 {
        return Err(MALFORMED);
    }
    let table = |flags: u8| {
        if flags & 0x80 != 0 {
            3usize << ((flags & 7) + 1)
        } else {
            0
        }
    };
    let mut i = 13 + table(b[10]);
    let mut out = b.get(..i).ok_or(MALFORMED)?.to_vec();
    let blocks = |mut p: usize| -> Result<usize, UploadError> {
        while p < b.len() {
            let length = b[p] as usize;
            p += 1;
            if length == 0 {
                return Ok(p);
            }
            p += length;
        }
        Err(MALFORMED)
    };
    while i < b.len() {
        let start = i;
        match b[i] {
            0x3b => {
                out.push(0x3b);
                return Ok(out);
            }
            0x2c => {
                if i + 10 > b.len() {
                    return Err(MALFORMED);
                }
                i = blocks(i + 11 + table(b[i + 9]))?;
                out.extend_from_slice(&b[start..i]);
            }
            0x21 => {
                let label = b.get(i + 1).copied();
                i += 2;
                match label {
                    Some(0xf9) => {
                        if b.get(i) != Some(&4) || i + 6 > b.len() || b[i + 5] != 0 {
                            return Err(MALFORMED);
                        }
                        i += 6;
                        out.extend_from_slice(&b[start..i]);
                    }
                    Some(0xff) => {
                        if b.get(i) != Some(&11) || i + 12 > b.len() {
                            return Err(MALFORMED);
                        }
                        let app = &b[i + 1..i + 12];
                        let data = i + 12;
                        i = blocks(data)?;
                        if app == b"NETSCAPE2.0" || app == b"ANIMEXTS1.0" {
                            if b.get(data) != Some(&3)
                                || b.get(data + 1) != Some(&1)
                                || data + 5 > b.len()
                            {
                                return Err(MALFORMED);
                            }
                            out.extend_from_slice(&b[start..data + 4]);
                            out.push(0);
                        }
                    }
                    _ => i = blocks(i)?,
                }
            }
            _ => return Err(MALFORMED),
        }
    }
    Err(MALFORMED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32fast::hash(&out[4..]);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    fn png(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in chunks {
            out.extend(png_chunk(kind, data));
        }
        out
    }

    fn ihdr() -> Vec<u8> {
        let mut ihdr = 2u32.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        ihdr
    }

    #[test]
    fn png_keeps_rendering_chunks_and_a_snapshot() {
        let ihdr = ihdr();
        let source = png(&[
            (b"IHDR", &ihdr),
            (b"tEXt", b"Author\0me"),
            (b"eXIf", b"MM"),
            (b"pHYs", &[0; 9]),
            (b"acTL", &[0; 8]),
            (b"IDAT", b"data"),
            (b"IEND", b""),
        ]);
        assert_eq!(
            clean_png(&source, None, true).unwrap(),
            png(&[
                (b"IHDR", &ihdr),
                (b"acTL", &[0; 8]),
                (b"IDAT", b"data"),
                (b"IEND", b""),
            ])
        );
        let snapshot = png_chunk(b"tEXt", b"buzz_agent_snapshot\0{}");
        assert_eq!(
            snapshot_chunk(
                &png_chunks(&png(&[
                    (b"IHDR", &ihdr),
                    (b"tEXt", b"buzz_agent_snapshot\0{}"),
                    (b"IEND", b"")
                ]))
                .unwrap()
            ),
            Some(&snapshot[..])
        );
        let cleaned = clean_png(&source, Some(&snapshot), false).unwrap();
        assert_eq!(
            &cleaned[8 + 12 + ihdr.len()..][..snapshot.len()],
            &snapshot[..]
        );
        assert_eq!(
            clean_png(&source[..source.len() - 4], None, false),
            Err(MALFORMED)
        );
        // An animation whose profile would change its colours is refused.
        let profiled = png(&[
            (b"IHDR", &ihdr),
            (b"iCCP", b"p\0\0x"),
            (b"acTL", &[0; 8]),
            (b"IEND", b""),
        ]);
        assert!(matches!(
            clean_png(&profiled, None, true),
            Err(UploadError::Prepare(_))
        ));
    }

    #[test]
    fn gif_keeps_frames_and_looping_only() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]); // 1x1, 2-colour table
        gif.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        let looping = [
            0x21, 0xff, 11, b'N', b'E', b'T', b'S', b'C', b'A', b'P', b'E', b'2', b'.', b'0', 3, 1,
            0, 0, 0,
        ];
        let xmp = [
            0x21, 0xff, 11, b'X', b'M', b'P', b' ', b'D', b'a', b't', b'a', b'X', b'M', b'P', 2,
            b'h', b'i', 0,
        ];
        let comment = [0x21, 0xfe, 3, b'y', b'o', b'u', 0];
        let text = [0x21, 0x01, 1, b'x', 0];
        let control = [0x21, 0xf9, 4, 0, 0, 0, 0, 0];
        let image = [0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0];
        for part in [
            &looping[..],
            &xmp,
            &comment,
            &text,
            &control,
            &image,
            &[0x3b],
        ] {
            gif.extend_from_slice(part);
        }
        let clean = clean_gif(&gif).unwrap();
        let mut expected = gif[..19].to_vec();
        for part in [&looping[..], &control, &image, &[0x3b]] {
            expected.extend_from_slice(part);
        }
        assert_eq!(clean, expected);
        assert_eq!(clean_gif(&gif[..gif.len() - 1]), Err(MALFORMED));
    }

    #[test]
    fn webp_loses_metadata_chunks_and_flags() {
        let mut vp8x = vec![0x08 | 0x20 | 0x10, 0, 0, 0];
        vp8x.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut body = b"WEBP".to_vec();
        body.extend(riff_chunk(b"VP8X", &vp8x));
        body.extend(riff_chunk(b"VP8L", b"pix"));
        body.extend(riff_chunk(b"EXIF", b"MMexif"));
        body.extend(riff_chunk(b"XMP ", b"<x/>"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&(body.len() as u32).to_le_bytes());
        webp.extend(body);
        let clean = clean_webp(&webp, false).unwrap();
        assert_eq!(sniff_image(&clean), Some("image/webp"));
        assert!(!clean.windows(4).any(|w| w == b"EXIF" || w == b"XMP "));
        assert_eq!(clean[20], 0x10, "only the alpha flag stays");
        assert_eq!(
            u32::from_le_bytes(clean[4..8].try_into().unwrap()) as usize,
            clean.len() - 8
        );
        // An unoriented EXIF needs no re-rendering.
        assert!(!webp_needs_transform(&webp_chunks(&webp).unwrap()));
    }

    /// A big-endian EXIF block with one orientation entry.
    fn exif(orientation: u16) -> Vec<u8> {
        let mut out = b"MM\0\x2a\0\0\0\x08\0\x01".to_vec();
        out.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1]);
        out.extend_from_slice(&orientation.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        out
    }

    /// A JPEG section: marker, big-endian length, payload.
    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let length = (payload.len() + 2) as u16;
        let mut out = vec![0xff, marker];
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn jpeg_parts() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut jfif = b"JFIF\0".to_vec();
        jfif.extend_from_slice(&[1, 1, 0, 0, 1, 0, 1, 0, 0]);
        let mut adobe = b"Adobe".to_vec();
        adobe.extend_from_slice(&[0, 100, 0, 0, 0, 0, 1]);
        let mut scan = segment(0xda, &[1, 1, 0, 0, 63, 0]);
        scan.extend_from_slice(&[42, 0xff, 0, 7, 0xff, 0xd0, 8]);
        (segment(0xe0, &jfif), segment(0xee, &adobe), scan)
    }

    #[test]
    fn encoded_jpegs_lose_metadata_between_scans_but_keep_their_pixels() {
        let (jfif, adobe, scan) = jpeg_parts();
        let mut pixels = vec![0xff, 0xd8];
        for part in [&jfif, &adobe, &segment(0xdb, &[1, 2]), &scan, &scan] {
            pixels.extend_from_slice(part);
        }
        pixels.extend_from_slice(&[0xff, 0xd9]);
        let mut dirty = vec![0xff, 0xd8];
        for part in [
            jfif.clone(),
            adobe.clone(),
            segment(0xe1, b"Exif\0private"),
            segment(0xe2, b"ICC_PROFILE\0private"),
            segment(0xed, b"Photoshop 3.0\0private"),
            segment(0xe0, b"noncanonical"),
            segment(0xee, b"noncanonical"),
            segment(0xdb, &[1, 2]),
            scan.clone(),
            segment(0xfe, b"private comment"),
            scan.clone(),
        ] {
            dirty.extend_from_slice(&part);
        }
        dirty.extend_from_slice(&[0xff, 0xd9]);
        dirty.extend_from_slice(b"trailing private data");
        assert_eq!(clean_encoded_jpeg(&dirty).unwrap(), pixels);
    }

    #[test]
    fn malformed_encoder_output_is_refused() {
        let (_, _, scan) = jpeg_parts();
        let mut unterminated = vec![0xff, 0xd8];
        unterminated.extend_from_slice(&scan);
        for bytes in [
            vec![],
            vec![0xff, 0xd8],
            vec![0xff, 0xd8, 0xff],
            vec![0xff, 0xd8, 0xff, 0xe1, 0, 1],
            vec![0xff, 0xd8, 0xff, 0xe1, 0, 20, 1],
            unterminated,
        ] {
            assert_eq!(clean_encoded_jpeg(&bytes), Err(MALFORMED), "{bytes:?}");
        }
    }

    #[test]
    fn rendered_jpegs_carry_no_metadata() {
        let image = image::RgbImage::from_pixel(4, 2, image::Rgb([200, 30, 30]));
        let mut source = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut source, 90)
            .encode_image(&image)
            .unwrap();
        let rendered = render(&source, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(clean_encoded_jpeg(&rendered).unwrap(), rendered);
    }

    #[test]
    fn reads_exif_orientation() {
        assert!(oriented(&exif(6)));
        assert!(!oriented(&exif(1)));
        let mut prefixed = b"Exif\0\0".to_vec();
        prefixed.extend(exif(3));
        assert!(oriented(&prefixed));
        assert!(!oriented(b"MM"));
    }

    #[test]
    fn still_images_are_rendered_with_their_orientation() {
        // A 4x2 PNG turned a quarter by its EXIF comes out 2x4.
        let image = image::RgbaImage::from_pixel(4, 2, image::Rgba([200, 10, 10, 255]));
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
            .unwrap();
        let chunks = png_chunks(&encoded).unwrap();
        let mut rotated = encoded[..8].to_vec();
        for chunk in &chunks {
            rotated.extend_from_slice(chunk.raw);
            if &chunk.kind == b"IHDR" {
                rotated.extend(png_chunk(b"eXIf", &exif(6)));
                rotated.extend(png_chunk(b"tEXt", b"Comment\0secret"));
            }
        }
        let prepared = prepare_image(&rotated, "turned.png").unwrap();
        assert_eq!(prepared.mime, "image/png");
        let decoded = image::load_from_memory(&prepared.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 4));
        assert!(
            !prepared
                .bytes
                .windows(4)
                .any(|w| w == b"eXIf" || w == b"tEXt")
        );

        let rgb = image::RgbImage::from_pixel(4, 3, image::Rgb([200, 10, 10]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&rgb)
            .unwrap();
        let prepared = prepare_image(&jpeg, "photo.jpg").unwrap();
        assert_eq!(sniff_image(&prepared.bytes), Some("image/jpeg"));
    }

    #[test]
    fn wide_gamut_pixels_are_converted_to_srgb() {
        let profile = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        let mut pixels = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        to_srgb(&profile, &mut pixels);
        // Display P3's red is outside sRGB: it clips, with no green or blue.
        assert_eq!(pixels.get_pixel(0, 0)[0], 255);
        let mut green = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 255, 0, 255]));
        to_srgb(&profile, &mut green);
        assert!(green.get_pixel(0, 0)[0] == 0 && green.get_pixel(0, 0)[1] == 255);
        // A broken profile changes nothing.
        let mut same = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        to_srgb(b"junk", &mut same);
        assert_eq!(same.get_pixel(0, 0).0, [1, 2, 3, 255]);
    }

    #[test]
    fn recognizes_containers_like_the_reference() {
        let ftyp = |brand: &[u8; 4], compatible: &[u8; 4]| {
            let mut out = 24u32.to_be_bytes().to_vec();
            out.extend_from_slice(b"ftyp");
            out.extend_from_slice(brand);
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(compatible);
            out.extend_from_slice(b"mp41");
            out
        };
        assert_eq!(video_demuxer(&ftyp(b"isom", b"avc1")), Some("mov"));
        assert_eq!(video_demuxer(&ftyp(b"qt  ", b"qt  ")), Some("mov"));
        assert_eq!(video_demuxer(&ftyp(b"M4A ", b"isom")), None, "audio");
        assert_eq!(video_demuxer(&ftyp(b"heic", b"mif1")), None, "a still");
        assert_eq!(video_demuxer(&ftyp(b"isom", b"avif")), None, "a still");
        assert_eq!(video_demuxer(b"\x1a\x45\xdf\xa3rest"), Some("matroska"));
        assert_eq!(video_demuxer(b"RIFF\0\0\0\0AVI LIST"), Some("avi"));
        assert_eq!(video_demuxer(b"\0\0\x01\xba"), Some("mpeg"));
        assert_eq!(
            video_demuxer(b"\0\0\0\x08free\0\0\0\x10mdat\0\0\0\0\0\0\0\0"),
            Some("mov")
        );
        assert_eq!(video_demuxer(b"\x89PNG\r\n\x1a\n"), None);

        assert!(is_heic(&ftyp(b"heic", b"mif1"), "IMG.bin"));
        assert!(is_heic(b"", "IMG_0001.HEIC"));
        assert!(!is_heic(&ftyp(b"isom", b"avc1"), "clip.mp4"));

        assert!(is_voice_note("voice-note-123.WAV"));
        assert!(!is_voice_note("voice-note-123.mp4"));
        assert_eq!(audio_demuxer(b"RIFF\0\0\0\0WAVEfmt "), Some("wav"));
        assert_eq!(audio_demuxer(b"ID3\x04"), Some("mp3"));
        assert_eq!(audio_demuxer(b"OggS"), Some("ogg"));
        assert_eq!(audio_demuxer(b"nothing"), None);

        assert_eq!(
            conversion("a.mov", "video/quicktime", &ftyp(b"qt  ", b"qt  ")),
            Ok(Some(Conversion::Video("mov")))
        );
        assert_eq!(conversion("a.heic", "", b""), Ok(Some(Conversion::Heic)));
        assert_eq!(
            conversion("a.webm", "video/webm", b"\x1a\x45\xdf\xa3"),
            Ok(Some(Conversion::Video("matroska")))
        );
        assert_eq!(
            conversion("a.ogv", "video/ogg", b"OggS"),
            Err(UploadError::Video)
        );
        assert_eq!(
            conversion("voice-note-1.wav", "audio/wav", b"junk"),
            Err(UploadError::Video)
        );
        assert_eq!(conversion("a.pdf", "application/pdf", b"%PDF"), Ok(None));
        assert_eq!(Conversion::Voice("wav").mode(), "voice:wav");
    }

    #[test]
    fn maps_relay_and_host_refusals() {
        assert_eq!(mime_type("a.PDF", b"%PDF"), "application/pdf");
        assert_eq!(mime_type("x.bin", b"\x89PNG\r\n\x1a\nrest"), "image/png");
        assert_eq!(mime_type("noext", b""), "application/octet-stream");
        assert_eq!(
            failure(400, r#"{"code":"capacity"}"#),
            UploadError::Capacity
        );
        assert_eq!(failure(503, r#"{"code":"ffmpeg"}"#), UploadError::Ffmpeg);
        assert_eq!(failure(400, r#"{"code":"io"}"#), UploadError::Io);
        assert_eq!(failure(422, "EXIF metadata refused"), UploadError::Metadata);
        assert_eq!(failure(415, "nope"), UploadError::Rejected);
        assert_eq!(failure(500, ""), UploadError::Failed);
        assert!(matches!(
            read_upload_response(403, ""),
            Err(UploadError::Denied)
        ));
        assert!(matches!(
            read_upload_response(413, ""),
            Err(UploadError::Size)
        ));
        assert!(matches!(
            read_upload_response(200, "not json"),
            Err(UploadError::Invalid)
        ));
        assert_eq!(host_error("Uploads are busy"), UploadError::Capacity);
        assert_eq!(host_error("Upload cancelled"), UploadError::Cancelled);
        assert_eq!(
            host_error("Media preparation could not access temporary storage"),
            UploadError::Io
        );
    }
}
