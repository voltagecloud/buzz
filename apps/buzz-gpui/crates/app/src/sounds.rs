//! Notification sounds (buzz-app's `src/features/notifications/sound.ts`).
//!
//! The twelve alert sounds buzz-app bundles (see NOTICE.md), chosen per
//! category in Settings → Notifications. Banners stay silent; the app plays
//! the sound itself, from memory, through GStreamer. Playback is best
//! effort: a missing decoder or audio device is logged once per sound and
//! never shown.

use crate::notifications::Preferences;
use buzz_relay_client::Category;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// An alert sound, or none. Serialized as buzz-app's sound names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sound {
    Bong,
    Boo,
    Dng,
    Doo,
    Doodone,
    Doong,
    Doop,
    Flirl,
    #[default]
    Flutter,
    OhNo,
    Ping,
    Unison,
    Silent,
}

impl Sound {
    /// Every audible sound, in buzz-app's `SOUND_NAMES` order.
    pub const ALL: [Sound; 12] = [
        Sound::Bong,
        Sound::Boo,
        Sound::Dng,
        Sound::Doo,
        Sound::Doodone,
        Sound::Doong,
        Sound::Doop,
        Sound::Flirl,
        Sound::Flutter,
        Sound::OhNo,
        Sound::Ping,
        Sound::Unison,
    ];

    /// The name buzz-app shows and stores.
    pub fn name(self) -> &'static str {
        match self {
            Sound::Bong => "bong",
            Sound::Boo => "boo",
            Sound::Dng => "dng",
            Sound::Doo => "doo",
            Sound::Doodone => "doodone",
            Sound::Doong => "doong",
            Sound::Doop => "doop",
            Sound::Flirl => "flirl",
            Sound::Flutter => "flutter",
            Sound::OhNo => "oh-no",
            Sound::Ping => "ping",
            Sound::Unison => "unison",
            Sound::Silent => "silent",
        }
    }

    /// The reference client's suggestion for a category.
    pub fn recommended(category: Category) -> Sound {
        match category {
            Category::Direct => Sound::Unison,
            Category::Mention => Sound::Ping,
            Category::Thread => Sound::Doop,
        }
    }

    /// A category's select options: Silent, the recommended sound, then the
    /// rest by name (`AlertSoundRow`).
    pub fn choices(category: Category) -> Vec<Sound> {
        let recommended = Sound::recommended(category);
        let mut rest: Vec<Sound> = Sound::ALL
            .into_iter()
            .filter(|sound| *sound != recommended)
            .collect();
        rest.sort_by_key(|sound| sound.name());
        [Sound::Silent, recommended]
            .into_iter()
            .chain(rest)
            .collect()
    }

    fn bytes(self) -> Option<&'static [u8]> {
        Some(match self {
            Sound::Bong => include_bytes!("../../../assets/sounds/bong.mp3"),
            Sound::Boo => include_bytes!("../../../assets/sounds/boo.mp3"),
            Sound::Dng => include_bytes!("../../../assets/sounds/dng.mp3"),
            Sound::Doo => include_bytes!("../../../assets/sounds/doo.mp3"),
            Sound::Doodone => include_bytes!("../../../assets/sounds/doodone.mp3"),
            Sound::Doong => include_bytes!("../../../assets/sounds/doong.mp3"),
            Sound::Doop => include_bytes!("../../../assets/sounds/doop.mp3"),
            Sound::Flirl => include_bytes!("../../../assets/sounds/flirl.mp3"),
            Sound::Flutter => include_bytes!("../../../assets/sounds/flutter.mp3"),
            Sound::OhNo => include_bytes!("../../../assets/sounds/oh-no.mp3"),
            Sound::Ping => include_bytes!("../../../assets/sounds/ping.mp3"),
            Sound::Unison => include_bytes!("../../../assets/sounds/unison.mp3"),
            Sound::Silent => return None,
        })
    }
}

/// Plays the sound the preferences choose for an alert in `category`, if
/// alerts, sound and the category are on. A sound already playing starts
/// over, as buzz-app rewinds its cached `Audio`.
pub fn play(category: Category, preferences: &Preferences) {
    if let Some(sound) = preferences.sound_for(category) {
        Player::global().start(Slot::Alert(sound), sound);
    }
}

/// Plays a plugin category's alert: the default sound, as buzz-app's
/// `resolveCategorySound` gives categories without a choice.
pub fn play_default(preferences: &Preferences) {
    if preferences.enabled && preferences.sound {
        let sound = Sound::default();
        Player::global().start(Slot::Alert(sound), sound);
    }
}

/// Plays `sound` as the Settings preview, stopping an earlier preview.
pub fn preview(sound: Sound) {
    let player = Player::global();
    player.stop(Slot::Preview);
    player.start(Slot::Preview, sound);
}

/// Stops the Settings preview, if one is playing.
pub fn stop_preview() {
    Player::global().stop(Slot::Preview);
}

/// What a pipeline plays for: one per alert sound, and one preview.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Alert(Sound),
    Preview,
}

/// Pipelines still playing, so a replay can stop them.
struct Player {
    playing: Mutex<HashMap<Slot, gst::Element>>,
}

impl Player {
    fn global() -> &'static Player {
        static PLAYER: OnceLock<Player> = OnceLock::new();
        PLAYER.get_or_init(|| Player {
            playing: Mutex::new(HashMap::new()),
        })
    }

    fn stop(&self, slot: Slot) {
        let previous = self
            .playing
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&slot));
        if let Some(pipeline) = previous {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }

    fn start(&'static self, slot: Slot, sound: Sound) {
        let Some(bytes) = sound.bytes() else {
            return;
        };
        if let Err(error) = crate::gst_bundle::init() {
            tracing::warn!(%error, "GStreamer is unavailable; notification sounds are off");
            return;
        }
        let pipeline = match pipeline(bytes) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                tracing::warn!(sound = sound.name(), %error, "could not play a sound");
                return;
            }
        };
        let Some(bus) = pipeline.bus() else {
            return;
        };
        if let Ok(mut map) = self.playing.lock()
            && let Some(previous) = map.insert(slot, pipeline.clone())
        {
            let _ = previous.set_state(gst::State::Null);
        }
        if pipeline.set_state(gst::State::Playing).is_err() {
            tracing::warn!(sound = sound.name(), "could not start a sound");
        }
        // The pipeline is released when it ends, fails or is replaced.
        std::thread::spawn(move || {
            let limit = gst::ClockTime::from_seconds(10);
            if let Some(message) =
                bus.timed_pop_filtered(limit, &[gst::MessageType::Eos, gst::MessageType::Error])
                && let gst::MessageView::Error(error) = message.view()
            {
                tracing::warn!(sound = sound.name(), error = %error.error(), "a sound failed");
            }
            let _ = pipeline.set_state(gst::State::Null);
            if let Ok(mut map) = self.playing.lock()
                && map.get(&slot) == Some(&pipeline)
            {
                map.remove(&slot);
            }
        });
    }
}

/// A `playbin` reading the whole file from memory.
fn pipeline(bytes: &'static [u8]) -> Result<gst::Element, String> {
    let playbin = gst::ElementFactory::make("playbin")
        .property("uri", "appsrc://")
        .build()
        .map_err(|error| error.to_string())?;
    playbin.set_property_from_str("flags", "audio+soft-volume");
    playbin.connect("source-setup", false, move |values| {
        if let Some(source) = values
            .get(1)
            .and_then(|value| value.get::<gst::Element>().ok())
            .and_then(|element| element.downcast::<gst_app::AppSrc>().ok())
        {
            source.set_stream_type(gst_app::AppStreamType::Stream);
            source.set_size(bytes.len() as i64);
            let _ = source.push_buffer(gst::Buffer::from_slice(bytes));
            let _ = source.end_of_stream();
        }
        None
    });
    Ok(playbin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long decoding a sound may take; alerts are about a second.
    const PLAY_LIMIT: u64 = 10_000;

    #[test]
    fn choices_follow_the_reference_order() {
        let names: Vec<&str> = Sound::choices(Category::Direct)
            .into_iter()
            .map(Sound::name)
            .collect();
        assert_eq!(
            names,
            [
                "silent", "unison", "bong", "boo", "dng", "doo", "doodone", "doong", "doop",
                "flirl", "flutter", "oh-no", "ping"
            ]
        );
        assert_eq!(Sound::choices(Category::Mention)[1], Sound::Ping);
        assert_eq!(Sound::choices(Category::Thread)[1], Sound::Doop);
    }

    #[test]
    fn names_round_trip_as_buzz_app_stores_them() {
        for sound in Sound::ALL.into_iter().chain([Sound::Silent]) {
            let json = serde_json::to_string(&sound).unwrap();
            assert_eq!(json, format!("\"{}\"", sound.name()));
            assert_eq!(serde_json::from_str::<Sound>(&json).unwrap(), sound);
        }
    }

    /// Every bundled file decodes to the end. Skips where GStreamer or an
    /// MP3 decoder is missing.
    #[test]
    fn bundled_sounds_decode() {
        if crate::gst_bundle::init().is_err()
            || gst::ElementFactory::find("mpegaudioparse").is_none()
        {
            eprintln!("GStreamer MP3 support missing; skipping");
            return;
        }
        for sound in Sound::ALL {
            let playbin = pipeline(sound.bytes().unwrap()).unwrap();
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
                .unwrap();
            playbin.set_property("audio-sink", &sink);
            let bus = playbin.bus().unwrap();
            playbin.set_state(gst::State::Playing).unwrap();
            let message = bus
                .timed_pop_filtered(
                    gst::ClockTime::from_mseconds(PLAY_LIMIT),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .expect("the sound ends");
            assert!(
                matches!(message.view(), gst::MessageView::Eos(_)),
                "{} failed: {message:?}",
                sound.name()
            );
            playbin.set_state(gst::State::Null).unwrap();
        }
    }
}
