//! Attachment media: images, custom emoji and profile pictures, plus the
//! audio and video players and the media review viewer.
//!
//! GPUI's own URI loader goes through the app's HTTP client and cannot sign
//! the Blossom `get` authorization relay media needs (relay.md §5n.6), so
//! every remote image loads through the community's [`Media`] client as an
//! [`ImageSource::Custom`] instead. GPUI's asset cache keeps one load per
//! URL and redraws the view that asked once it lands.

pub(crate) mod attachments;
mod audio;
mod comments;
mod controls;
mod draft_viewer;
mod engine;
mod preview;
pub mod recorder;
mod review;
mod seek;
mod timecode;
mod video;

pub use attachments::{RowMedia, attachments, thread_media};
pub use audio::{
    audio_attachment, forget_local, local_bytes, pause_audio, set_local, voice_draft, voice_note,
};
// The composer's draft thumbnails use this.
pub use engine::probe_file;
pub use review::{dismiss_review, review_overlay, viewer_shown};
// The composer opens its draft attachments here.
pub use draft_viewer::tile_key as draft_tile_key;
pub use preview::{ImageStatus, Open, image_preview, image_status};
pub use review::{draft_tile_hidden, open_draft_preview, track_tile};
pub use review::{open_media, open_media_at};
pub use timecode::body as text;
pub use video::{OpenReview, video_preview};

use buzz_relay_client::Media;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Blurhash previews decode to this many pixels a side (`BLURHASH_SIZE`).
const BLURHASH_SIZE: u32 = 32;

/// One remote image request. Hashing by URL alone lets every view share a
/// load; a media client is tied to one community, and so is its URL.
#[derive(Clone)]
struct Remote {
    url: SharedString,
    /// Tried when `url` fails: avatars ask for the relay thumbnail first.
    fallback: Option<SharedString>,
    media: Media,
}

impl Hash for Remote {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.url.hash(state);
        self.fallback.hash(state);
    }
}

enum RemoteImage {}

impl Asset for RemoteImage {
    type Source = Remote;
    type Output = Result<Arc<Image>, SharedString>;

    // `async fn` would borrow the `App` argument past the call.
    #[allow(clippy::manual_async_fn)]
    fn load(source: Remote, _: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        async move {
            let bytes = match source.media.fetch(&source.url).await {
                Ok(bytes) => bytes,
                Err(error) => match &source.fallback {
                    Some(fallback) => source.media.fetch(fallback).await,
                    None => Err(error),
                }
                .map_err(|error| SharedString::from(error.to_string()))?,
            };
            let format = image_format(&bytes).ok_or("Not a supported image")?;
            // Relay media never renders SVG (`media_type`); other hosts'
            // images render as a browser would.
            if format == ImageFormat::Svg && source.media.is_relay_media(&source.url) {
                return Err("Not a supported image".into());
            }
            Ok(Arc::new(Image::from_bytes(format, bytes.to_vec())))
        }
    }
}

/// An image inline in a `data:image/<format>[;base64],…` URL, for the
/// formats the app draws.
pub fn inline_image(url: &str) -> Option<ImageSource> {
    let format = match url.strip_prefix("data:image/")?.split([';', ',']).next()? {
        "svg+xml" => ImageFormat::Svg,
        "png" => ImageFormat::Png,
        "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        _ => return None,
    };
    let bytes = buzz_relay_client::community_icon::inline_bytes(url)?;
    Some(ImageSource::Image(Arc::new(Image::from_bytes(
        format, bytes,
    ))))
}

/// An image at `url`, at full size.
pub fn image(url: &str, media: &Media) -> ImageSource {
    source(Remote {
        url: url.to_string().into(),
        fallback: None,
        media: media.clone(),
    })
}

/// A profile picture: the relay's small thumbnail when it is relay media
/// (`mediaUrl(url, "small")`), falling back to the original.
pub fn avatar(url: &str, media: &Media) -> ImageSource {
    source(avatar_remote(url, media))
}

fn avatar_remote(url: &str, media: &Media) -> Remote {
    match media.thumbnail(url) {
        Some(thumbnail) => Remote {
            url: thumbnail.into(),
            fallback: Some(url.to_string().into()),
            media: media.clone(),
        },
        None => Remote {
            url: url.to_string().into(),
            fallback: None,
            media: media.clone(),
        },
    }
}

/// A profile picture from whichever community it belongs to: fetched
/// through the community whose media it is, which signs for it, else the
/// current one's (other hosts' images need no signature).
pub fn any_avatar(url: &str) -> ImageSource {
    let url = url.to_string();
    ImageSource::Custom(Arc::new(move |window, cx| {
        let Some(media) = media_for(&url, cx) else {
            return Some(Err(ImageCacheError::Asset("No community".into())));
        };
        decoded(&avatar_remote(&url, &media), window, cx)
            .map(|result| result.map_err(ImageCacheError::Asset))
    }))
}

/// The media client for `url`: its own community's, else the current one.
fn media_for(url: &str, cx: &App) -> Option<Media> {
    let state = cx.try_global::<crate::community::AppState>()?;
    state
        .communities
        .iter()
        .map(|community| community.read(cx).session.media())
        .find(|media| media.is_relay_media(url))
        .cloned()
        .or_else(|| {
            crate::community::AppState::current(cx)
                .map(|community| community.read(cx).session.media().clone())
        })
}

fn source(remote: Remote) -> ImageSource {
    ImageSource::Custom(Arc::new(move |window, cx| {
        decoded(&remote, window, cx).map(|result| result.map_err(ImageCacheError::Asset))
    }))
}

/// The decoded image, `None` while it loads.
fn decoded(
    remote: &Remote,
    window: &mut Window,
    cx: &mut App,
) -> Option<Result<Arc<RenderImage>, SharedString>> {
    match window.use_asset::<RemoteImage>(remote, cx)? {
        Ok(image) => image.use_render_image(window, cx).map(Ok),
        Err(error) => Some(Err(error)),
    }
}

/// The pixel size of the image at `url` once it has loaded.
fn natural_size(url: &str, media: &Media, window: &mut Window, cx: &mut App) -> Option<Size<f32>> {
    let remote = Remote {
        url: url.to_string().into(),
        fallback: None,
        media: media.clone(),
    };
    let image = decoded(&remote, window, cx)?.ok()?;
    let size = image.size(0);
    Some(gpui_kit::size(size.width.0 as f32, size.height.0 as f32))
}

/// Packs BGRA pixels into the image type GPUI paints.
pub(crate) fn render_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// A blurhash decoded to a small image (`paintBlurhash`). The hash was
/// validated when the event was folded.
enum Blurhash {}

impl Asset for Blurhash {
    type Source = SharedString;
    type Output = Option<Arc<RenderImage>>;

    #[allow(clippy::manual_async_fn)]
    fn load(
        hash: SharedString,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        async move { blurhash_image(&hash) }
    }
}

fn blurhash_image(hash: &str) -> Option<Arc<RenderImage>> {
    let mut pixels = blurhash::decode(hash, BLURHASH_SIZE, BLURHASH_SIZE, 1.).ok()?;
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    render_image(BLURHASH_SIZE, BLURHASH_SIZE, pixels)
}

/// A thumbnail's pixels (`ImagePixels`): the blurhash, when there is one,
/// until the original is decoded, which then replaces it at once. If the
/// original fails, the blurhash (or the tile's own fill) stays. Fills its
/// parent with `object-fit: cover`.
pub fn thumbnail(url: &str, blurhash: Option<&str>, media: &Media) -> impl IntoElement {
    let remote = Remote {
        url: url.to_string().into(),
        fallback: None,
        media: media.clone(),
    };
    let hash = blurhash.map(SharedString::from);
    Lazy::new("pixels", move |window, cx| {
        let image = decoded(&remote, window, cx).and_then(Result::ok);
        let preview = image
            .is_none()
            .then(|| {
                hash.as_ref()
                    .and_then(|hash| window.use_asset::<Blurhash>(hash, cx).flatten())
            })
            .flatten();
        div()
            .relative()
            .size_full()
            .when_some(preview, |this, preview| {
                this.child(img(preview).absolute().inset_0().size_full())
            })
            .when_some(image, |this, image| {
                this.child(
                    img(image)
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Cover),
                )
            })
            .into_any_element()
    })
}

/// Builds its content during layout, where the window is available for
/// asset loads and per-element state; row renderers only get `&App`.
pub struct Lazy {
    id: ElementId,
    build: Option<Build>,
}

type Build = Box<dyn FnOnce(&mut Window, &mut App) -> AnyElement>;

impl Lazy {
    pub fn new(
        id: impl Into<ElementId>,
        build: impl FnOnce(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            build: Some(Box::new(build)),
        }
    }
}

impl IntoElement for Lazy {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Lazy {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let build = self.build.take().expect("Lazy laid out twice");
        let mut element = build(window, cx);
        (element.request_layout(window, cx), element)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

/// `formatMediaTime`: `m:ss`, or `h:mm:ss` from an hour.
fn media_time(seconds: f64) -> String {
    let total = if seconds.is_finite() {
        seconds.max(0.).floor() as u64
    } else {
        0
    };
    let (hours, minutes, rest) = (total / 3600, total % 3600 / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{rest:02}")
    } else {
        format!("{minutes}:{rest:02}")
    }
}

/// `videoTime`: [`media_time`] padded to `00:00`.
fn video_time(seconds: f64) -> String {
    format!("{:0>5}", media_time(seconds))
}

/// Tabular figures, so running times do not jitter.
fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// Sniffs the formats GPUI decodes from their magic bytes. Senders' MIME
/// claims are not trusted.
fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    let starts = |magic: &[u8]| bytes.starts_with(magic);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if starts(b"\xff\xd8\xff") {
        Some(ImageFormat::Jpeg)
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && starts(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if starts(b"BM") {
        Some(ImageFormat::Bmp)
    } else if starts(b"II*\0") || starts(b"MM\0*") {
        Some(ImageFormat::Tiff)
    } else if starts(b"\0\0\x01\0") {
        Some(ImageFormat::Ico)
    } else if is_svg(bytes) {
        Some(ImageFormat::Svg)
    } else {
        None
    }
}

/// An SVG document: optional BOM, XML declaration, comments or doctype,
/// then an `<svg` root.
fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let Ok(text) = std::str::from_utf8(head)
        .or_else(|error| std::str::from_utf8(&head[..error.valid_up_to()]))
    else {
        return false;
    };
    let mut rest = text.trim_start_matches('\u{feff}').trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("<?") {
            let Some(end) = after.find("?>") else {
                return false;
            };
            rest = after[end + 2..].trim_start();
        } else if let Some(after) = rest.strip_prefix("<!--") {
            let Some(end) = after.find("-->") else {
                return false;
            };
            rest = after[end + 3..].trim_start();
        } else if let Some(after) = rest.strip_prefix("<!") {
            let Some(end) = after.find('>') else {
                return false;
            };
            rest = after[end + 1..].trim_start();
        } else {
            return rest.starts_with("<svg");
        }
    }
}

/// Saves an attachment into Downloads without replacing anything there
/// (`media_download`), and returns where.
pub(super) async fn save(
    media: &Media,
    url: &str,
    name: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    let directory = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
    media
        .download(url, name, directory)
        .await
        .inspect_err(|error| {
            tracing::warn!(%error, "attachment download failed");
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn sniffs_formats() {
        assert_eq!(
            image_format(b"\x89PNG\r\n\x1a\nrest"),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            image_format(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        assert_eq!(image_format(b"GIF89a"), Some(ImageFormat::Gif));
    }

    #[test]
    fn sniffs_svg_documents() {
        assert_eq!(
            image_format(b"<?xml version=\"1.0\"?>\n<!-- c -->\n<svg xmlns=\"x\"/>"),
            Some(ImageFormat::Svg)
        );
        assert_eq!(
            image_format(b"<!DOCTYPE svg>\n<svg/>"),
            Some(ImageFormat::Svg)
        );
        assert_eq!(image_format(b"<html><svg/>"), None);
    }

    #[test]
    fn formats_media_times_like_the_reference() {
        assert_eq!(media_time(0.), "0:00");
        assert_eq!(media_time(65.9), "1:05");
        assert_eq!(media_time(3_725.), "1:02:05");
        assert_eq!(media_time(f64::NAN), "0:00");
        assert_eq!(video_time(8.03), "00:08");
        assert_eq!(video_time(605.), "10:05");
        assert_eq!(video_time(3_725.), "1:02:05");
    }

    #[test]
    fn decodes_blurhash_previews() {
        let image = blurhash_image("LEHV6nWB2yk8pyo0adR*.7kCMdnj").expect("a valid hash decodes");
        assert_eq!(image.size(0), size(DevicePixels(32), DevicePixels(32)));
        // Swapped to BGRA, the order GPUI paints.
        let rgba = blurhash::decode("LEHV6nWB2yk8pyo0adR*.7kCMdnj", 32, 32, 1.).unwrap();
        let bgra = &image.as_bytes(0).unwrap()[..4];
        assert_eq!(bgra, [rgba[2], rgba[1], rgba[0], rgba[3]]);
    }
}
