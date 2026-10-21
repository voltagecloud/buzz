//! The GStreamer that ships inside Buzz.app, and a self-check of the
//! GStreamer the app finds.
//!
//! The macOS bundle carries GStreamer's libraries in
//! `Contents/Frameworks`, its plugins in `Contents/Resources/lib/gstreamer-1.0`,
//! GIO's modules in `Contents/Resources/lib/gio/modules` and the plugin
//! scanner in
//! `Contents/MacOS` (see `scripts/package-macos.sh`). [`use_bundled`] points
//! GStreamer and GIO there before anything initializes them, so a
//! GStreamer the user installed (Homebrew, the official framework) is never
//! mixed in. Windows needs none of this: GStreamer finds its plugins
//! relative to its own DLL.
//!
//! `buzz-gpui --gst-check [URI...]` runs [`self_check`]: it lists where each
//! element the app uses comes from, encodes a voice note, and plays that
//! and every URI given to the end, then exits. Packaging runs it against
//! the finished bundle.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::OnceLock;
use std::time::Duration;

/// Initializes GStreamer once for the whole app, and says whether it can
/// be used. The first call loads the plugin registry, which can take a
/// moment, so [`warm_up`] makes it at launch, off the UI thread.
pub fn init() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| gst::init().map_err(|error| error.to_string()))
        .clone()
}

/// Starts [`init`] on a thread of its own, so the first sound or video
/// does not wait for it on the UI thread.
pub fn warm_up() {
    let spawned = std::thread::Builder::new()
        .name("gstreamer-init".into())
        .spawn(|| {
            if let Err(error) = init() {
                tracing::warn!(%error, "GStreamer is unavailable");
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start GStreamer early");
    }
}

/// Called first thing in `main`, before any other thread starts.
pub fn use_bundled() {
    #[cfg(target_os = "macos")]
    {
        let Some(contents) = bundle_contents() else {
            return;
        };
        let registry = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("buzz-gpui")
            .join(format!("gstreamer-registry-{}.bin", std::env::consts::ARCH));
        // SAFETY: no other thread exists yet to read the environment.
        unsafe {
            // Plugins only from the bundle, never from a path the user set.
            for name in [
                "GST_PLUGIN_PATH",
                "GST_PLUGIN_PATH_1_0",
                "GST_PLUGIN_SYSTEM_PATH",
                "GIO_EXTRA_MODULES",
            ] {
                std::env::remove_var(name);
            }
            std::env::set_var(
                "GST_PLUGIN_SYSTEM_PATH_1_0",
                contents.join("Resources/lib/gstreamer-1.0"),
            );
            std::env::set_var(
                "GST_PLUGIN_SCANNER_1_0",
                contents.join("MacOS/gst-plugin-scanner"),
            );
            // Apart from the registry of any other GStreamer on the machine.
            std::env::set_var("GST_REGISTRY_1_0", registry);
            std::env::set_var("GIO_MODULE_DIR", contents.join("Resources/lib/gio/modules"));
        }
    }
}

/// `Buzz.app/Contents` when this executable runs from a bundle that
/// carries GStreamer.
#[cfg(target_os = "macos")]
fn bundle_contents() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let contents = exe.parent()?.parent()?;
    contents
        .join("Resources/lib/gstreamer-1.0")
        .is_dir()
        .then(|| contents.to_path_buf())
}

/// Every element playback, sounds and voice notes reach for, directly or
/// through `playbin`, for the codecs attachments use (MP4/H.264/AAC,
/// QuickTime/MPEG-4, WebM/VP8/VP9/AV1, Ogg/Opus/Vorbis, MP3, FLAC, WAV).
const ELEMENTS: &[&str] = &[
    "playbin",
    "uridecodebin",
    "decodebin",
    "typefind",
    "appsrc",
    "appsink",
    "fakesink",
    "filesrc",
    "filesink",
    "souphttpsrc",
    "audioconvert",
    "audioresample",
    "videoconvert",
    "videoscale",
    "volume",
    "autoaudiosink",
    "autoaudiosrc",
    #[cfg(target_os = "macos")]
    "osxaudiosink",
    #[cfg(target_os = "macos")]
    "osxaudiosrc",
    "qtdemux",
    "matroskademux",
    "oggdemux",
    "wavparse",
    "id3demux",
    "h264parse",
    "aacparse",
    "mpegaudioparse",
    "avdec_h264",
    "avdec_aac",
    "avdec_mpeg4",
    "mpg123audiodec",
    "vp8dec",
    "vp9dec",
    "dav1ddec",
    "opusdec",
    "vorbisdec",
    "flacdec",
    "avenc_aac",
    "mp4mux",
];

/// How long one clip may take to play through.
const PLAY_TIMEOUT: Duration = Duration::from_secs(60);

/// `--gst-check`: see the module docs. False on any failure.
pub fn self_check(uris: &[String]) -> bool {
    if let Err(error) = init() {
        println!("FAIL GStreamer did not initialize: {error}");
        return false;
    }
    println!("{}", gst::version_string());
    let mut failed = false;

    #[cfg(target_os = "macos")]
    let bundle = bundle_contents();
    #[cfg(not(target_os = "macos"))]
    let bundle: Option<std::path::PathBuf> = None;
    if let Some(contents) = &bundle {
        println!("bundle {}", contents.display());
    }
    for plugin in gst::Registry::get().plugins() {
        let file = plugin.filename();
        let shown = file
            .as_ref()
            .map(|file| file.display().to_string())
            .unwrap_or_else(|| "(built in)".into());
        let foreign = match (&bundle, &file) {
            (Some(contents), Some(file)) => !file.starts_with(contents),
            _ => false,
        };
        if foreign {
            println!(
                "FAIL plugin {} is outside the bundle: {shown}",
                plugin.plugin_name()
            );
            failed = true;
        }
    }
    for name in ELEMENTS {
        match gst::ElementFactory::find(name) {
            Some(factory) => {
                let from = factory
                    .plugin()
                    .and_then(|plugin| plugin.filename())
                    .map(|file| file.display().to_string())
                    .unwrap_or_else(|| "(built in)".into());
                println!("ok   {name} from {from}");
            }
            None => {
                println!("FAIL {name} is missing");
                failed = true;
            }
        }
    }

    // A voice note as the composer encodes it, then played back: AAC and
    // the H.264 track through mp4mux, qtdemux and the libav decoders.
    let rate = crate::ui::media::recorder::SAMPLE_RATE;
    let tone: Vec<i16> = (0..rate)
        .map(|i| {
            ((f64::from(i) / f64::from(rate) * 330. * std::f64::consts::TAU).sin() * 12_000.) as i16
        })
        .collect();
    let mut clips = Vec::new();
    let voice_note =
        std::env::temp_dir().join(format!("buzz-gst-check-{}.mp4", std::process::id()));
    match crate::ui::media::recorder::encode_mp4(&tone)
        .and_then(|mp4| std::fs::write(&voice_note, mp4).map_err(|error| error.to_string()))
    {
        Ok(()) => match url::Url::from_file_path(&voice_note) {
            Ok(uri) => clips.push(uri.to_string()),
            Err(()) => {
                println!("FAIL no URI for {}", voice_note.display());
                failed = true;
            }
        },
        Err(error) => {
            println!("FAIL voice note encoding: {error}");
            failed = true;
        }
    }
    clips.extend(uris.iter().cloned());
    for uri in &clips {
        match play(uri) {
            Ok(()) => println!("ok   played {uri}"),
            Err(error) => {
                println!("FAIL {uri}: {error}");
                failed = true;
            }
        }
    }
    let _ = std::fs::remove_file(&voice_note);

    println!(
        "GStreamer self-check {}",
        if failed { "failed" } else { "passed" }
    );
    !failed
}

/// Plays `uri` to the end as fast as it decodes, video converted to the
/// BGRA the app draws. Any stream without a decoder fails it.
fn play(uri: &str) -> Result<(), String> {
    let sink = |description: &str| {
        gst::parse::bin_from_description(description, true)
            .map(|bin| bin.upcast::<gst::Element>())
            .map_err(|error| error.to_string())
    };
    let playbin = gst::ElementFactory::make("playbin")
        .property("uri", uri)
        .property(
            "video-sink",
            sink("videoconvert ! video/x-raw,format=BGRA ! fakesink sync=false")?,
        )
        .property(
            "audio-sink",
            sink("audioconvert ! audioresample ! fakesink sync=false")?,
        )
        .build()
        .map_err(|error| error.to_string())?;
    let bus = playbin.bus().ok_or("playbin has no bus")?;
    let result = (|| {
        playbin
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        let deadline = std::time::Instant::now() + PLAY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err("timed out".to_string());
            }
            let Some(message) =
                bus.timed_pop(gst::ClockTime::from_nseconds(left.as_nanos() as u64))
            else {
                continue;
            };
            match message.view() {
                gst::MessageView::Eos(_) => return Ok(()),
                gst::MessageView::Error(error) => {
                    return Err(format!("{} ({:?})", error.error(), error.debug()));
                }
                // A stream nothing decodes. playbin also asks for the
                // optional `deinterlace` element, which neither GStreamer's
                // macOS nor its Windows build carries; it plays without.
                gst::MessageView::Element(element) => {
                    let Some(structure) = element.structure() else {
                        continue;
                    };
                    if structure.name() != "missing-plugin" {
                        continue;
                    }
                    let kind = structure.get::<String>("type").unwrap_or_default();
                    if kind == "element" {
                        println!("note {uri}: optional element missing: {structure}");
                    } else {
                        return Err(format!("missing plugin: {structure}"));
                    }
                }
                _ => {}
            }
        }
    })();
    let _ = playbin.set_state(gst::State::Null);
    result
}
