//! Voice note capture and encoding (`useVoiceNoteRecorder.ts`,
//! `voiceNoteWav.ts`, the host's `media_preparation.rs`).
//!
//! The microphone records 16-bit mono at 24 kHz, as the reference's WAV
//! does, with a level every 120 ms for the live waveform. A finished note
//! becomes what buzz-app uploads: an MP4 with AAC audio and a black 16x16
//! H.264 track, because the relay accepts audio only inside video. Ubuntu's
//! GStreamer packages carry an AAC encoder (gst-libav) but no H.264 one,
//! so the black frame is a fixed, pre-encoded picture repeated once a
//! second.
//!
//! Development only: `BUZZ_DEV_FAKE_MIC` records a test signal instead of
//! the microphone (its value picks an `audiotestsrc` wave, e.g.
//! `pink-noise`; `1` means a sine).

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 24_000;
/// `VOICE_NOTE_MAX_DURATION_SECONDS`.
pub const MAX_SECONDS: f64 = 300.;
/// `VOICE_NOTE_WAVEFORM_INTERVAL_MS` worth of samples per level.
const LEVEL_SAMPLES: usize = SAMPLE_RATE as usize * 120 / 1000;
/// `summarizeWaveform(channel, 48)`.
pub const WAVEFORM_BUCKETS: usize = 48;
const MAX_SAMPLES: usize = SAMPLE_RATE as usize * MAX_SECONDS as usize;
const ENCODE_TIMEOUT: Duration = Duration::from_secs(120);
/// The still video's frame rate. The upload re-encodes it at a constant
/// rate, so its last frame lasts a whole frame; the relay refuses an
/// `imeta` duration more than 0.1s from that track, which 1 fps overshot.
const VIDEO_FPS: u32 = 25;

pub const NO_MICROPHONE: &str = "Buzz could not start the voice recorder.";
pub const UNAVAILABLE: &str = "Voice recording is not available in this environment.";
pub const DENIED: &str = "Allow Buzz to access your microphone to record a voice note.";
pub const INTERRUPTED: &str = "The voice recording was interrupted. Record it again.";
pub const NOT_PREPARED: &str = "Buzz could not prepare this voice note for upload.";

/// A black 16x16 baseline frame from x264 (`color=c=black:s=16x16:r=1`):
/// its parameter sets and the one IDR slice.
const SPS: &[u8] = &[
    0x67, 0x42, 0xc0, 0x0a, 0xdd, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x03,
    0x00, 0x83, 0xc4, 0x89, 0xe0,
];
const PPS: &[u8] = &[0x68, 0xce, 0x0f, 0x2c, 0x80];
const IDR: &[u8] = &[
    0x65, 0x88, 0x84, 0x04, 0xbc, 0x98, 0xa0, 0x00, 0x38, 0xa3, 0x80,
];

#[derive(Default)]
struct Captured {
    samples: Vec<i16>,
    levels: Vec<f32>,
    sum: f64,
    count: usize,
}

impl Captured {
    fn push(&mut self, bytes: &[u8]) {
        for pair in bytes.as_chunks::<2>().0 {
            let sample = i16::from_le_bytes(*pair);
            if self.samples.len() < MAX_SAMPLES {
                self.samples.push(sample);
            }
            let value = sample as f64 / 32768.;
            self.sum += value * value;
            self.count += 1;
            if self.count == LEVEL_SAMPLES {
                self.levels
                    .push(level((self.sum / self.count as f64).sqrt()));
                self.sum = 0.;
                self.count = 0;
            }
        }
    }
}

/// The analyser's RMS as a 0-1 level (`min(1, rms * 5.5)`).
fn level(rms: f64) -> f32 {
    (rms * 5.5).min(1.) as f32
}

/// A recording in progress. Dropping it stops the microphone.
pub struct Recorder {
    pipeline: gst::Element,
    bus: gst::Bus,
    captured: Arc<Mutex<Captured>>,
    started: Instant,
}

fn source() -> String {
    match std::env::var("BUZZ_DEV_FAKE_MIC") {
        Ok(wave) => {
            let wave = if wave.is_empty() || wave == "1" {
                "sine"
            } else {
                wave.as_str()
            };
            format!("audiotestsrc is-live=true wave={wave} freq=330 volume=0.6")
        }
        Err(_) => "autoaudiosrc".into(),
    }
}

impl Recorder {
    pub fn start() -> Result<Self, String> {
        // No GStreamer, or no audio source plugin, means no recorder here.
        crate::gst_bundle::init().map_err(|_| UNAVAILABLE.to_string())?;
        let description = format!(
            "{} ! audioconvert ! audioresample ! \
             audio/x-raw,format=S16LE,rate={SAMPLE_RATE},channels=1,layout=interleaved ! \
             appsink name=sink sync=false",
            source()
        );
        let pipeline = gst::parse::launch(&description).map_err(|error| {
            if error.matches(gst::ParseError::NoSuchElement) {
                UNAVAILABLE.to_string()
            } else {
                NO_MICROPHONE.to_string()
            }
        })?;
        let sink = pipeline
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.by_name("sink"))
            .and_then(|sink| sink.downcast::<gst_app::AppSink>().ok())
            .ok_or(NO_MICROPHONE)?;
        let captured = Arc::new(Mutex::new(Captured::default()));
        let shared = captured.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    if let Ok(mut captured) = shared.lock() {
                        captured.push(map.as_slice());
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let bus = pipeline.bus().ok_or(NO_MICROPHONE)?;
        if pipeline.set_state(gst::State::Playing).is_err() {
            // A failed change can leave it part way; it must reach Null
            // before it is freed.
            let _ = pipeline.set_state(gst::State::Null);
            return Err(NO_MICROPHONE.into());
        }
        Ok(Self {
            pipeline,
            bus,
            captured,
            started: Instant::now(),
        })
    }

    /// Seconds recorded so far; zero until the microphone delivers.
    pub fn seconds(&self) -> f64 {
        self.captured
            .lock()
            .map_or(0., |c| c.samples.len() as f64 / SAMPLE_RATE as f64)
    }

    /// The microphone has delivered sound.
    pub fn live(&self) -> bool {
        self.captured
            .lock()
            .is_ok_and(|c| !c.samples.is_empty() || c.count > 0)
    }

    /// How many levels have been measured.
    pub fn level_count(&self) -> usize {
        self.captured.lock().map_or(0, |c| c.levels.len())
    }

    /// The newest `count` levels, oldest first.
    pub fn levels(&self, count: usize) -> Vec<f32> {
        self.captured.lock().map_or(Vec::new(), |c| {
            c.levels[c.levels.len().saturating_sub(count)..].to_vec()
        })
    }

    /// A pipeline error, or a microphone that never started.
    pub fn failure(&self) -> Option<String> {
        while let Some(message) = self.bus.pop() {
            if let gst::MessageView::Error(error) = message.view() {
                tracing::warn!(error = %error.error(), "voice recording failed");
                // A sandbox or sound server refusing the microphone.
                if error.error().matches(gst::ResourceError::NotAuthorized) {
                    return Some(DENIED.into());
                }
                return Some(if self.live() {
                    INTERRUPTED.into()
                } else {
                    NO_MICROPHONE.into()
                });
            }
        }
        (!self.live() && self.started.elapsed() > Duration::from_secs(10))
            .then(|| NO_MICROPHONE.into())
    }

    /// Stops and returns the samples, at most five minutes.
    pub fn finish(self) -> Vec<i16> {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.captured
            .lock()
            .map(|mut c| std::mem::take(&mut c.samples))
            .unwrap_or_default()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub fn duration(samples: &[i16]) -> f64 {
    samples.len() as f64 / SAMPLE_RATE as f64
}

/// `summarizeWaveform` then `round(n * 100)`: the peak of each of
/// `buckets` equal slices, 0-100.
pub fn waveform(samples: &[i16], buckets: usize) -> Vec<u8> {
    if samples.is_empty() || buckets == 0 {
        return Vec::new();
    }
    (0..buckets)
        .map(|bucket| {
            let start = bucket * samples.len() / buckets;
            let end = ((bucket + 1) * samples.len() / buckets).max(start + 1);
            let peak = samples[start..end.min(samples.len())]
                .iter()
                .map(|s| (*s as f32 / 32768.).abs())
                .fold(0f32, f32::max)
                .min(1.);
            (peak * 100.).round() as u8
        })
        .collect()
}

/// A live waveform bar: 3 to 20px (`audio.ts:140-146`).
pub fn bar_height(level: f32) -> f32 {
    3. + ((level.min(1.) - 0.16).max(0.) / 0.84 * 17.).round()
}

/// `AVCDecoderConfigurationRecord` for the black frame.
fn avc_config() -> Vec<u8> {
    let mut config = vec![1, SPS[1], SPS[2], SPS[3], 0xff, 0xe1];
    config.extend_from_slice(&(SPS.len() as u16).to_be_bytes());
    config.extend_from_slice(SPS);
    config.push(1);
    config.extend_from_slice(&(PPS.len() as u16).to_be_bytes());
    config.extend_from_slice(PPS);
    config
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Encodes a recording as the MP4 buzz-app uploads: AAC at 128 kbit/s and
/// the black video track, metadata first (`+faststart`). Blocks; run it
/// off the UI thread.
pub fn encode_mp4(samples: &[i16]) -> Result<Vec<u8>, String> {
    if samples.is_empty() {
        return Err(NOT_PREPARED.into());
    }
    // filesink opens its path by name and follows links, so the file goes
    // in a fresh directory only this user can enter, not the shared one.
    let bytes = tempfile::Builder::new()
        .prefix("buzz-voice-")
        .tempdir()
        .map_err(|error| error.to_string())
        .and_then(|dir| {
            let path = dir.path().join("voice.mp4");
            encode_to(samples, &path)?;
            std::fs::read(&path).map_err(|error| error.to_string())
        })
        .map(|mut mp4| {
            blank_user_data(&mut mp4, 0, None);
            mp4
        });
    bytes.map_err(|error| {
        tracing::warn!(%error, "voice note encoding failed");
        NOT_PREPARED.to_string()
    })
}

/// Turns every `udta` box into `free` padding of the same size. The
/// relay refuses user data other than ffmpeg's empty one, and mp4mux
/// writes its own; renaming keeps every offset, so `stco` stays right.
fn blank_user_data(mp4: &mut [u8], start: usize, end: Option<usize>) {
    const CONTAINERS: [&[u8; 4]; 7] = [
        b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"dinf",
    ];
    let end = end.unwrap_or(mp4.len()).min(mp4.len());
    let mut offset = start;
    while offset + 8 <= end {
        let size = u32::from_be_bytes([
            mp4[offset],
            mp4[offset + 1],
            mp4[offset + 2],
            mp4[offset + 3],
        ]) as usize;
        if size < 8 || offset + size > end {
            return;
        }
        let kind = [
            mp4[offset + 4],
            mp4[offset + 5],
            mp4[offset + 6],
            mp4[offset + 7],
        ];
        if &kind == b"udta" {
            mp4[offset + 4..offset + 8].copy_from_slice(b"free");
        } else if CONTAINERS.contains(&&kind) {
            blank_user_data(mp4, offset + 8, Some(offset + size));
        }
        offset += size;
    }
}

fn encode_to(samples: &[i16], path: &std::path::Path) -> Result<(), String> {
    crate::gst_bundle::init()?;
    let location = path.to_str().ok_or("temporary path")?;
    let description = format!(
        "appsrc name=audio format=time \
           caps=audio/x-raw,format=S16LE,rate={SAMPLE_RATE},channels=1,layout=interleaved \
           ! audioconvert ! audioresample ! avenc_aac bitrate=128000 ! aacparse ! mux.audio_0 \
         appsrc name=video format=time \
           caps=\"video/x-h264,stream-format=avc,alignment=au,width=16,height=16,\
                 framerate={VIDEO_FPS}/1,codec_data=(buffer){}\" ! mux.video_0 \
         mp4mux name=mux faststart=true ! filesink location=\"{location}\"",
        hex(&avc_config())
    );
    let pipeline = gst::parse::launch(&description).map_err(|error| error.to_string())?;
    // Every early return below stops it.
    let pipeline = super::engine::Stopping(pipeline);
    let bin = pipeline
        .0
        .downcast_ref::<gst::Bin>()
        .ok_or("pipeline is not a bin")?;
    let source = |name: &str| {
        bin.by_name(name)
            .and_then(|element| element.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| format!("no {name} source"))
    };
    let (audio, video) = (source("audio")?, source("video")?);
    let bus = pipeline.0.bus().ok_or("no bus")?;
    pipeline
        .0
        .set_state(gst::State::Playing)
        .map_err(|error| error.to_string())?;

    let seconds = duration(samples);
    let fps = VIDEO_FPS as f64;
    let frames = (seconds * fps).ceil().max(1.) as u64;
    for frame in 0..frames {
        let mut access_unit = (IDR.len() as u32).to_be_bytes().to_vec();
        access_unit.extend_from_slice(IDR);
        let mut buffer = gst::Buffer::from_mut_slice(access_unit);
        let buffer_ref = buffer.get_mut().ok_or("buffer")?;
        let start = frame as f64 / fps;
        // The last frame ends with the audio.
        let end = ((frame + 1) as f64 / fps).min(seconds).max(start + 0.001);
        let (start, end) = (
            gst::ClockTime::from_nseconds((start * 1e9) as u64),
            gst::ClockTime::from_nseconds((end * 1e9) as u64),
        );
        buffer_ref.set_pts(start);
        buffer_ref.set_dts(start);
        buffer_ref.set_duration(end.saturating_sub(start));
        video
            .push_buffer(buffer)
            .map_err(|error| error.to_string())?;
    }
    video.end_of_stream().map_err(|error| error.to_string())?;

    const CHUNK: usize = SAMPLE_RATE as usize / 5;
    for (index, chunk) in samples.chunks(CHUNK).enumerate() {
        let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let buffer_ref = buffer.get_mut().ok_or("buffer")?;
        let offset = (index * CHUNK) as u64;
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(
            offset * 1_000_000_000 / SAMPLE_RATE as u64,
        ));
        buffer_ref.set_duration(gst::ClockTime::from_nseconds(
            chunk.len() as u64 * 1_000_000_000 / SAMPLE_RATE as u64,
        ));
        audio
            .push_buffer(buffer)
            .map_err(|error| error.to_string())?;
    }
    audio.end_of_stream().map_err(|error| error.to_string())?;

    let deadline = Instant::now() + ENCODE_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err("timed out".to_string());
        }
        let wait = gst::ClockTime::from_mseconds(left.as_millis().min(500) as u64);
        let Some(message) = bus.timed_pop(wait) else {
            continue;
        };
        match message.view() {
            gst::MessageView::Eos(_) => break Ok(()),
            gst::MessageView::Error(error) => break Err(error.error().to_string()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn tone(seconds: f64) -> Vec<i16> {
        (0..(seconds * SAMPLE_RATE as f64) as usize)
            .map(|i| {
                let t = i as f64 / SAMPLE_RATE as f64;
                ((t * 330. * std::f64::consts::TAU).sin() * 12_000.) as i16
            })
            .collect()
    }

    #[test]
    fn summarizes_levels_and_waveforms_like_the_reference() {
        assert_eq!(bar_height(0.), 3.);
        assert_eq!(bar_height(0.16), 3.);
        assert_eq!(bar_height(1.), 20.);
        assert_eq!(bar_height(4.), 20.);
        let mut captured = Captured::default();
        let silence = vec![0u8; LEVEL_SAMPLES * 2];
        captured.push(&silence);
        let loud: Vec<u8> = tone(0.12).iter().flat_map(|s| s.to_le_bytes()).collect();
        captured.push(&loud);
        assert_eq!(captured.levels.len(), 2);
        assert_eq!(captured.levels[0], 0.);
        assert_eq!(captured.levels[1], 1.);

        let mut samples = vec![0i16; 4800];
        samples.extend(tone(0.2));
        let waveform = waveform(&samples, WAVEFORM_BUCKETS);
        assert_eq!(waveform.len(), WAVEFORM_BUCKETS);
        assert_eq!(waveform[0], 0);
        assert!(waveform[WAVEFORM_BUCKETS - 1] > 30);
        assert!(waveform.iter().all(|w| *w <= 100));
    }

    #[test]
    fn encodes_the_mp4_the_relay_accepts() {
        crate::gst_bundle::init().unwrap();
        if gst::ElementFactory::find("avenc_aac").is_none() {
            // A system without gst-libav, or a stale registry cache that
            // blacklisted it (`GST_REGISTRY` points elsewhere).
            eprintln!("avenc_aac is unavailable; skipping");
            return;
        }
        let mp4 = encode_mp4(&tone(2.5)).unwrap();
        // ISO-BMFF with the movie header before the media data.
        assert_eq!(&mp4[4..8], b"ftyp");
        let atoms: Vec<&[u8]> = {
            let mut atoms = Vec::new();
            let mut offset = 0;
            while offset + 8 <= mp4.len() {
                let size = u32::from_be_bytes(mp4[offset..offset + 4].try_into().unwrap()) as usize;
                atoms.push(&mp4[offset + 4..offset + 8]);
                if size < 8 {
                    break;
                }
                offset += size;
            }
            atoms
        };
        let position = |name: &[u8]| atoms.iter().position(|atom| *atom == name);
        assert!(position(b"moov").unwrap() < position(b"mdat").unwrap());
        // Both tracks: H.264 video and AAC audio.
        let has = |needle: &[u8]| mp4.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"avc1") && has(b"avcC") && has(b"mp4a"));
        assert!(!has(b"udta"), "no user data for the relay to refuse");
        let probe = super::super::engine::probe(mp4.into()).unwrap();
        let length = probe.duration.unwrap();
        assert!((length - 2.5).abs() < 0.6, "{length}");
    }

    /// The relay validates the upload as a video and the `imeta` with its
    /// duration. Skipped unless `BUZZ_TEST_RELAY` points at `.test-relay`;
    /// sends to a throwaway channel.
    #[test]
    fn sends_a_voice_note_through_the_test_relay() {
        use buzz_relay_client::lifecycle::NewChannel;
        use buzz_relay_client::upload::{self, UploadFile};
        use buzz_relay_client::{Mentions, RelayOrigin, RosterStatus, Session, Store, fold};
        let Ok(dir) = std::env::var("BUZZ_TEST_RELAY") else {
            eprintln!("BUZZ_TEST_RELAY not set; skipping");
            return;
        };
        crate::gst_bundle::init().unwrap();
        if gst::ElementFactory::find("avenc_aac").is_none() {
            eprintln!("avenc_aac is unavailable; skipping");
            return;
        }
        let identities: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{dir}/identities.json")).unwrap(),
        )
        .unwrap();
        let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
        let alice = std::sync::Arc::new(
            buzz_identity::Identity::from_nsec(
                identities["identities"]["alice"]["nsec"].as_str().unwrap(),
            )
            .unwrap(),
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let round_trip = async move {
            let wait = Duration::from_secs(20);
            let session = Session::open(
                &tokio::runtime::Handle::current(),
                origin,
                alice,
                Store::in_memory().unwrap(),
            );
            let mut snapshot = session.snapshot();
            tokio::time::timeout(
                wait,
                snapshot.wait_for(|s| s.roster == RosterStatus::Ready && s.live),
            )
            .await
            .unwrap()
            .unwrap();
            let channel = session
                .create_channel(NewChannel {
                    name: format!("#voice-{}", std::process::id()),
                    description: "Created by a test".into(),
                    private: false,
                    ttl: None,
                    labels: Vec::new(),
                })
                .await
                .unwrap();
            let samples = tone(1.5);
            let name = format!("voice-note-{}.mp4", 1_791_590_000_000u64);
            let mut attachment = upload::upload(
                session.media(),
                UploadFile {
                    name: name.clone(),
                    mime: "video/mp4".into(),
                    bytes: encode_mp4(&samples).unwrap(),
                },
            )
            .await
            .unwrap();
            assert_eq!(attachment.mime, "video/mp4");
            // Not a whole second: the relay holds the tag to the converted
            // video track's length.
            attachment.voice_duration = Some(duration(&samples));
            let view = session.open_channel(&channel);
            session
                .send(&channel, "", Mentions::default(), vec![attachment], None)
                .await
                .unwrap();
            let mut timeline = view.snapshot.clone();
            let seen = tokio::time::timeout(
                wait,
                timeline.wait_for(|t| {
                    t.rows.iter().any(|row| {
                        row.attachments.len() == 1
                            && row
                                .delivery
                                .as_ref()
                                .is_none_or(|d| d.status == fold::Delivery::Seen)
                    })
                }),
            )
            .await
            .unwrap()
            .unwrap()
            .clone();
            let row = seen.rows.iter().find(|r| r.attachments.len() == 1).unwrap();
            // The plain test relay serves media over HTTP, so the note shows
            // as an unavailable file here; over HTTPS it is a voice note.
            assert_eq!(row.attachments[0].name.as_deref(), Some(name.as_str()));
        };
        // Creating the channel, the upload and the send wait on the relay
        // with no deadline of their own (an upload may take ten minutes),
        // so a relay that stops answering fails here instead of stalling.
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(120), round_trip).await })
            .expect("the voice note went through the relay within two minutes");
    }
}
