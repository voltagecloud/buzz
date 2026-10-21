//! Audio and video decoding with GStreamer, which Ubuntu ships with the
//! codecs attachments use (MP4/H.264/AAC, WebM, Ogg/Opus, MP3, WAV).
//!
//! HTTPS relay media streams from buzz-desktop's loopback listener
//! ([`Media::stream_url`]), which reads ranges through its block cache, so
//! playback starts before the download finishes and seeking fetches only
//! what it needs. Anything else arrives whole through [`Media::fetch`] and
//! a `playbin` reads it from memory through a random-access `appsrc`.
//! Local files (composer drafts) play from their `file://` URI.
//!
//! Video frames come out of an `appsink` as BGRA, the layout GPUI's
//! `RenderImage` holds. Everything here is synchronous; the UI polls.
//!
//! [`Media::stream_url`]: buzz_relay_client::Media::stream_url
//! [`Media::fetch`]: buzz_relay_client::Media::fetch

use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Bytes pushed per `need-data` request at most.
const CHUNK: usize = 256 * 1024;
/// How long a poster frame may take to decode.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a pipeline plays.
#[derive(Clone)]
pub enum Source {
    /// A whole body in memory.
    Bytes(Arc<[u8]>),
    /// A URI GStreamer reads itself: the loopback stream or a local file.
    Uri(String),
}

impl Source {
    /// A local file, as a `file://` URI.
    pub fn file(path: &Path) -> Result<Self, String> {
        let path = path
            .canonicalize()
            .map_err(|_| "The file is unavailable".to_string())?;
        url::Url::from_file_path(&path)
            .map(|uri| Self::Uri(uri.into()))
            .map_err(|()| "The file is unavailable".to_string())
    }
}

impl From<Arc<[u8]>> for Source {
    fn from(bytes: Arc<[u8]>) -> Self {
        Self::Bytes(bytes)
    }
}

/// One decoded video frame, tightly packed BGRA.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Video,
    /// Video without sound, paused on its first frame.
    Poster,
    /// Sound into a null sink, only to read the length.
    Metadata,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// Prerolled: duration and seeking are available.
    Ready,
    Ended,
    Failed(String),
}

type Slot = Arc<Mutex<Option<Frame>>>;

pub struct Engine {
    playbin: gst::Element,
    bus: gst::Bus,
    frame: Option<Slot>,
    ready: bool,
    pending_seek: Option<f64>,
    rate: f64,
}

impl Engine {
    /// A paused pipeline over `source`. Errors (a missing decoder, a
    /// corrupt file, a failed read) arrive later as [`Event::Failed`].
    pub fn new(source: impl Into<Source>, kind: Kind) -> Result<Self, String> {
        crate::gst_bundle::init()?;
        let source = source.into();
        let uri = match &source {
            Source::Bytes(_) => "appsrc://",
            Source::Uri(uri) => uri.as_str(),
        };
        let playbin = gst::ElementFactory::make("playbin")
            .property("uri", uri)
            .build()
            .map_err(|error| error.to_string())?;
        if let Source::Bytes(bytes) = source {
            playbin.connect("source-setup", false, move |values| {
                if let Some(source) = values
                    .get(1)
                    .and_then(|value| value.get::<gst::Element>().ok())
                    .and_then(|element| element.downcast::<gst_app::AppSrc>().ok())
                {
                    feed(&source, bytes.clone());
                }
                None
            });
        }
        playbin.set_property_from_str(
            "flags",
            match kind {
                Kind::Audio | Kind::Metadata => "audio+soft-volume",
                Kind::Video => "video+audio+soft-volume",
                Kind::Poster => "video",
            },
        );
        let frame = matches!(kind, Kind::Video | Kind::Poster).then(|| {
            let slot = Slot::default();
            playbin.set_property("video-sink", video_sink(slot.clone()));
            slot
        });
        // Every pipeline names both sinks. Left unset, playbin opens the
        // system's devices for a stream its flags leave out (pulse, ALSA,
        // OSS for sound; X for pictures) while it autoplugs, and an open
        // that never returns wedges the streaming thread and with it the
        // state changes that would stop it.
        let null_sink = || {
            gst::ElementFactory::make("fakesink")
                .build()
                .map_err(|error| error.to_string())
        };
        match kind {
            Kind::Audio | Kind::Video => playbin.set_property("audio-sink", audio_sink()),
            Kind::Metadata | Kind::Poster => playbin.set_property("audio-sink", null_sink()?),
        }
        if matches!(kind, Kind::Audio | Kind::Metadata) {
            playbin.set_property("video-sink", null_sink()?);
        }
        let bus = playbin.bus().ok_or("The player has no bus")?;
        if playbin.set_state(gst::State::Paused).is_err() {
            // A failed change can leave it part way; it must reach Null
            // before it is freed.
            let _ = playbin.set_state(gst::State::Null);
            return Err("This media cannot be played".into());
        }
        Ok(Self {
            playbin,
            bus,
            frame,
            ready: false,
            pending_seek: None,
            rate: 1.,
        })
    }

    /// Drains the pipeline's messages.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(message) = self.bus.pop() {
            if let Some(event) = self.handle(&message) {
                events.push(event);
            }
        }
        events
    }

    fn handle(&mut self, message: &gst::Message) -> Option<Event> {
        use gst::MessageView;
        match message.view() {
            MessageView::Eos(_) => Some(Event::Ended),
            MessageView::Error(error) => {
                let detail = error.debug().map(|d| d.to_string()).unwrap_or_default();
                tracing::warn!(error = %error.error(), %detail, "media playback failed");
                Some(Event::Failed(error.error().to_string()))
            }
            MessageView::StateChanged(change)
                if !self.ready
                    && message.src() == Some(self.playbin.upcast_ref())
                    && change.current() >= gst::State::Paused =>
            {
                self.ready = true;
                // A speed chosen before preroll applies through a seek too.
                let pending = self.pending_seek.take();
                if pending.is_some() || self.rate != 1. {
                    self.seek(pending.unwrap_or(0.));
                }
                Some(Event::Ready)
            }
            _ => None,
        }
    }

    /// Starts playing; `false` when the pipeline refuses (`play()`
    /// rejecting in the reference).
    pub fn play(&self) -> bool {
        self.playbin.set_state(gst::State::Playing).is_ok()
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn pause(&self) {
        let _ = self.playbin.set_state(gst::State::Paused);
    }

    pub fn position(&self) -> Option<f64> {
        self.playbin
            .query_position::<gst::ClockTime>()
            .map(gst::ClockTime::seconds_f64)
    }

    pub fn duration(&self) -> Option<f64> {
        self.playbin
            .query_duration::<gst::ClockTime>()
            .map(gst::ClockTime::seconds_f64)
            .filter(|seconds| seconds.is_finite() && *seconds > 0.)
    }

    pub fn seek(&mut self, seconds: f64) {
        if !self.ready {
            self.pending_seek = Some(seconds);
            return;
        }
        let Some(at) = seek_target(seconds, self.duration()) else {
            return;
        };
        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
        let _ = self.playbin.seek(
            self.rate,
            flags,
            gst::SeekType::Set,
            at,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
    }

    /// Playback speed, kept across seeks.
    pub fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
        if self.ready
            && let Some(position) = self.position()
        {
            self.seek(position);
        }
    }

    /// Linear 0..=1.
    pub fn set_volume(&self, volume: f64) {
        self.playbin.set_property("volume", volume.clamp(0., 1.));
    }

    pub fn set_muted(&self, muted: bool) {
        self.playbin.set_property("mute", muted);
    }

    /// The newest frame not taken yet.
    pub fn take_frame(&self) -> Option<Frame> {
        self.frame.as_ref()?.lock().ok()?.take()
    }
}

/// Where a seek to `seconds` lands: inside the clip when its length is
/// known, and `None` for a time no clock can hold (a typed `[9999999:00]`).
fn seek_target(seconds: f64, duration: Option<f64>) -> Option<gst::ClockTime> {
    let seconds = duration.map_or(seconds, |duration| seconds.min(duration));
    gst::ClockTime::try_from_seconds_f64(seconds.max(0.)).ok()
}

impl Drop for Engine {
    /// A pipeline that never prerolled may never stop either: stopping
    /// waits for its streaming threads. Stop it elsewhere so dropping a
    /// player never blocks the UI.
    fn drop(&mut self) {
        stop_elsewhere(self.playbin.clone());
    }
}

fn stop_elsewhere(pipeline: gst::Element) {
    std::thread::spawn(move || {
        let _ = pipeline.set_state(gst::State::Null);
    });
}

/// A pipeline set to Null when it goes, so an early return cannot free
/// one still running.
pub(super) struct Stopping(pub gst::Element);

impl Drop for Stopping {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

/// What a video looks like before it plays: its first frame and length.
pub struct Probe {
    pub frame: Option<Frame>,
    pub duration: Option<f64>,
}

/// Decodes the first frame of a video. Blocks; run it off the UI thread.
#[cfg(test)]
pub fn probe(bytes: Arc<[u8]>) -> Result<Probe, String> {
    probe_source(Source::Bytes(bytes), Kind::Poster)
}

/// The first frame and length of a local video file, such as a composer
/// draft. Blocks; run it off the UI thread.
pub fn probe_file(path: &Path) -> Result<Probe, String> {
    probe_source(Source::file(path)?, Kind::Poster)
}

/// A clip's first frame (`Poster`) or only its length (`Metadata`), from
/// any source. Blocks; run it off the UI thread.
pub fn probe_source(source: Source, kind: Kind) -> Result<Probe, String> {
    let mut engine = Engine::new(source, kind)?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while !engine.ready {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            // Dropping stops it elsewhere, so the timeout still returns.
            return Err("Timed out decoding the video".into());
        }
        let wait = gst::ClockTime::from_mseconds(left.as_millis().min(200) as u64);
        if let Some(message) = engine.bus.timed_pop(wait) {
            match engine.handle(&message) {
                Some(Event::Failed(error)) => return Err(error),
                Some(Event::Ended) => break,
                _ => {}
            }
        }
    }
    Ok(Probe {
        frame: engine.take_frame(),
        duration: engine.duration(),
    })
}

/// A byte range of the shared body, so pushing never copies it.
struct Chunk(Arc<[u8]>, Range<usize>);

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        &self.0[self.1.clone()]
    }
}

/// Serves `bytes` to a random-access `appsrc`, so demuxers can seek (MP4
/// files often keep their index at the end).
fn feed(source: &gst_app::AppSrc, bytes: Arc<[u8]>) {
    let size = bytes.len();
    let offset = Arc::new(AtomicU64::new(0));
    source.set_stream_type(gst_app::AppStreamType::RandomAccess);
    source.set_size(size as i64);
    source.set_format(gst::Format::Bytes);
    let seek_offset = offset.clone();
    source.set_callbacks(
        gst_app::AppSrcCallbacks::builder()
            .need_data(move |source, length| {
                let start = (offset.load(Ordering::SeqCst) as usize).min(size);
                if start >= size {
                    let _ = source.end_of_stream();
                    return;
                }
                let want = if length == 0 || length == u32::MAX {
                    CHUNK
                } else {
                    (length as usize).min(CHUNK)
                };
                let end = (start + want).min(size);
                let mut buffer = gst::Buffer::from_slice(Chunk(bytes.clone(), start..end));
                if let Some(buffer) = buffer.get_mut() {
                    buffer.set_offset(start as u64);
                }
                offset.store(end as u64, Ordering::SeqCst);
                let _ = source.push_buffer(buffer);
            })
            .seek_data(move |_, position| {
                seek_offset.store(position, Ordering::SeqCst);
                true
            })
            .build(),
    );
}

fn video_sink(slot: Slot) -> gst_app::AppSink {
    let caps = gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Bgra)
        .pixel_aspect_ratio(gst::Fraction::new(1, 1))
        .build();
    let sink = gst_app::AppSink::builder()
        .caps(&caps)
        .max_buffers(1)
        .drop(true)
        .build();
    let preroll = slot.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_preroll(move |sink| {
                let sample = sink.pull_preroll().map_err(|_| gst::FlowError::Eos)?;
                store(&preroll, &sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                store(&slot, &sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    sink
}

fn store(slot: &Slot, sample: &gst::Sample) {
    if let Some(frame) = frame(sample)
        && let Ok(mut slot) = slot.lock()
    {
        *slot = Some(frame);
    }
}

fn frame(sample: &gst::Sample) -> Option<Frame> {
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer()?, &info).ok()?;
    let (width, height) = (frame.width(), frame.height());
    let stride = *frame.plane_stride().first()? as usize;
    let data = frame.plane_data(0).ok()?;
    let row = width as usize * 4;
    let mut bgra = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        bgra.extend_from_slice(data.get(y * stride..y * stride + row)?);
    }
    Some(Frame {
        width,
        height,
        bgra,
    })
}

/// The desktop's audio output, or a clock-synced null sink when there is
/// none, so playback still advances (and video still plays) without sound.
fn audio_sink() -> gst::Element {
    if let Ok(sink) = gst::ElementFactory::make("autoaudiosink").build()
        && sink.set_state(gst::State::Ready).is_ok()
    {
        let _ = sink.set_state(gst::State::Null);
        return sink;
    }
    gst::ElementFactory::make("fakesink")
        .property("sync", true)
        .build()
        .expect("fakesink is in GStreamer core")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../relay/tests/fixtures")
            .join(name)
    }

    #[test]
    fn seeks_stay_inside_the_clip_and_the_clock() {
        assert_eq!(seek_target(-3., None), Some(gst::ClockTime::ZERO));
        assert_eq!(
            seek_target(36e9, Some(90.)),
            Some(gst::ClockTime::from_seconds(90))
        );
        assert_eq!(seek_target(1e300, None), None);
    }

    #[test]
    fn probes_local_video_files() {
        let probe = probe_file(&fixture("clip.webm")).expect("the WebM decodes");
        let frame = probe.frame.expect("a first frame");
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(frame.bgra.len(), (frame.width * frame.height * 4) as usize);
        assert!(probe.duration.is_some_and(|d| d > 0.));
        assert!(probe_file(Path::new("/nonexistent/clip.mp4")).is_err());
    }

    #[test]
    fn reads_lengths_from_uris() {
        let source = Source::file(&fixture("clip.webm")).unwrap();
        assert!(matches!(&source, Source::Uri(uri) if uri.starts_with("file:///")));
        let probe = probe_source(source, Kind::Metadata).expect("the WebM decodes");
        assert!(probe.frame.is_none());
    }
}
