//! Inline audio: the plain player (`AudioAttachment.tsx`) and the voice
//! note card (`bundled/voice-notes/VoiceNoteCard.tsx`).
//!
//! One clip plays at a time (`claimAudio`), so playback lives in a single
//! app-wide [`Audio`] entity. Rows read it while rendering, which makes
//! GPUI redraw them as it ticks. HTTPS relay clips stream from the
//! loopback listener; others are fetched whole.

use super::Lazy;
use super::controls::hover_time;
use super::engine::{Engine, Event, Kind, Source};
use super::preview::{Unavailable, attachment_unavailable};
use super::seek::{OnHover, OnSeek, seek_area, seek_area_hover};
use super::video::finite_duration;
use super::{media_time, tabular, video_time};
use buzz_relay_client::Media;
use buzz_relay_client::fold::Attachment;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, ButtonVariant, Icon, IconButton, IconButtonSize, IconName,
    TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// How often a playing clip reports its position.
const TICK: Duration = Duration::from_millis(50);
/// Voice note speeds, in the order the button cycles them.
const VOICE_RATES: [f64; 4] = [1., 1.5, 2., 0.5];
/// Voice note bars before the track is measured, and at most.
const DEFAULT_BARS: usize = 38;
const MAX_BARS: usize = 64;
/// One spinner turn on the voice card (`spin 800ms`).
const SPIN: Duration = Duration::from_millis(800);
/// An ended clip this much shorter than its container claimed corrects
/// its length (old voice notes carried a 1 fps video track).
const CORRECTION_MIN: f64 = 0.05;
const CORRECTION_MAX: f64 = 1.5;

#[derive(Default)]
pub struct Audio {
    track: Option<Track>,
    /// Measured lengths, which win over the sender's `duration` hint.
    durations: HashMap<String, f64>,
    /// Lengths corrected when a clip ended short of them.
    corrected: HashMap<String, f64>,
    /// Clips whose length is being or has been read (`preload="metadata"`),
    /// kept after a failure too so a clip is read once, not every layout.
    probed: HashSet<String>,
    failed: HashSet<String>,
    rates: HashMap<String, f64>,
    /// Clips that play from memory, not the relay: voice note drafts.
    local: HashMap<String, Arc<[u8]>>,
    /// The seek bar under the mouse: `(url, position)`.
    hover: Option<(String, f32)>,
    /// Voice note waveform widths, by card.
    widths: HashMap<ElementId, f32>,
}

struct Track {
    url: String,
    voice: bool,
    /// The view whose row started it, which pauses it when it goes away.
    owner: Option<EntityId>,
    engine: Option<Engine>,
    /// The pipeline prerolled, so a paused clip has nothing left to read.
    ready: bool,
    playing: bool,
    ended: bool,
    position: f64,
    /// Loads the clip, then follows it while it plays.
    _task: Task<()>,
}

struct AudioGlobal(Entity<Audio>);

impl Global for AudioGlobal {}

/// What a player shows for one clip.
#[derive(Clone, Copy, Default)]
struct Status {
    playing: bool,
    loading: bool,
    position: f64,
    duration: Option<f64>,
    /// The clip's length was read: seeking works (`metadataReady`).
    measured: bool,
    failed: bool,
    rate: f64,
}

fn status(url: &str, tagged: Option<f64>, cx: &App) -> Status {
    let tagged = finite_duration(tagged);
    let Some(audio) = cx.try_global::<AudioGlobal>() else {
        return Status {
            duration: tagged,
            rate: 1.,
            ..Status::default()
        };
    };
    let audio = audio.0.read(cx);
    let track = audio.track.as_ref().filter(|track| track.url == url);
    let measured = audio
        .corrected
        .get(url)
        .or(audio.durations.get(url))
        .copied();
    Status {
        playing: track.is_some_and(|track| track.playing),
        loading: track.is_some_and(|track| track.engine.is_none() && track.playing),
        position: track.map_or(0., |track| track.position),
        duration: measured.or(tagged),
        measured: measured.is_some(),
        failed: audio.failed.contains(url),
        rate: audio.rates.get(url).copied().unwrap_or(1.),
    }
}

fn audio(cx: &mut App) -> Entity<Audio> {
    if let Some(audio) = cx.try_global::<AudioGlobal>() {
        return audio.0.clone();
    }
    let audio = cx.new(|_| Audio::default());
    cx.set_global(AudioGlobal(audio.clone()));
    // Rows drawn before it existed do not watch it yet.
    cx.refresh_windows();
    audio
}

/// Lets `key` play `bytes` from memory, for a draft that is not uploaded
/// yet.
pub fn set_local(key: &str, bytes: Arc<[u8]>, cx: &mut App) {
    let key = key.to_string();
    audio(cx).update(cx, |audio, _| {
        audio.local.insert(key, bytes);
    });
}

/// A draft's bytes from [`set_local`], for a plugin that draws it.
pub fn local_bytes(key: &str, cx: &App) -> Option<Arc<[u8]>> {
    cx.try_global::<AudioGlobal>()
        .and_then(|audio| audio.0.read(cx).local.get(key).cloned())
}

/// Forgets a draft from [`set_local`], stopping it if it plays.
pub fn forget_local(key: &str, cx: &mut App) {
    let Some(audio) = cx.try_global::<AudioGlobal>().map(|audio| audio.0.clone()) else {
        return;
    };
    audio.update(cx, |audio, cx| {
        audio.local.remove(key);
        if audio.track.as_ref().is_some_and(|track| track.url == key) {
            audio.track = None;
        }
        cx.notify();
    });
}

/// Pauses the playing clip when the conversation that started it is no
/// longer shown (`useConversationPresentation`): any clip for `None`, else
/// only one started from the view `owner`.
pub fn pause_audio(owner: Option<EntityId>, cx: &mut App) {
    let Some(audio) = cx.try_global::<AudioGlobal>().map(|audio| audio.0.clone()) else {
        return;
    };
    audio.update(cx, |audio, cx| {
        let Some(track) = audio.track.as_mut() else {
            return;
        };
        if owner.is_some() && track.owner != owner {
            return;
        }
        match track.engine.as_ref() {
            Some(engine) => {
                engine.pause();
                track.playing = false;
            }
            // Still loading: cancel.
            None => audio.track = None,
        }
        cx.notify();
    });
}

fn toggle(url: &str, voice: bool, media: &Media, owner: Option<EntityId>, cx: &mut App) {
    let (url, media) = (url.to_string(), media.clone());
    audio(cx).update(cx, |audio, cx| audio.toggle(url, voice, media, owner, cx));
}

fn seek(
    url: &str,
    voice: bool,
    media: &Media,
    seconds: f64,
    owner: Option<EntityId>,
    cx: &mut App,
) {
    let (url, media) = (url.to_string(), media.clone());
    audio(cx).update(cx, |audio, cx| {
        audio.seek(url, voice, media, seconds, owner, cx)
    });
}

/// Reads the length of a clip without a trusted one, once.
fn probe(url: &str, media: &Media, cx: &mut App) {
    let audio = audio(cx);
    let (url, media) = (url.to_string(), media.clone());
    audio.update(cx, |audio, cx| {
        if audio.durations.contains_key(&url) || !audio.probed.insert(url.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let source = match media.stream_url(&url) {
                Some(uri) => Ok(Source::Uri(uri)),
                None => media
                    .fetch(&url)
                    .await
                    .map(Source::Bytes)
                    .map_err(|error| error.to_string()),
            };
            let duration = match source {
                // A probe can block for its whole timeout; give it a
                // thread of its own, as video posters do.
                Ok(source) => {
                    let (sender, receiver) = futures::channel::oneshot::channel();
                    std::thread::spawn(move || {
                        let probe = super::engine::probe_source(source, Kind::Metadata);
                        let _ = sender.send(probe.ok().and_then(|probe| probe.duration));
                    });
                    receiver.await.ok().flatten()
                }
                Err(_) => None,
            };
            let _ = this.update(cx, |audio, cx| {
                if let Some(duration) = finite_duration(duration) {
                    audio.durations.entry(url).or_insert(duration);
                    cx.notify();
                }
            });
        })
        .detach();
    });
}

impl Audio {
    fn toggle(
        &mut self,
        url: String,
        voice: bool,
        media: Media,
        owner: Option<EntityId>,
        cx: &mut Context<Self>,
    ) {
        match self.track.as_mut().filter(|track| track.url == url) {
            // Pressing again while it loads cancels.
            Some(track) if track.engine.is_none() => self.track = None,
            Some(track) => {
                let engine = track.engine.as_mut().expect("loaded");
                track.owner = owner;
                if track.playing {
                    engine.pause();
                    track.playing = false;
                } else {
                    if track.ended {
                        engine.seek(0.);
                        track.position = 0.;
                        track.ended = false;
                    }
                    track.playing = engine.play();
                    if track.playing {
                        // Following stopped with the pause.
                        track._task = Self::follow(url.clone(), cx);
                    }
                }
            }
            None => self.start(url, voice, media, true, 0., owner, cx),
        }
        cx.notify();
    }

    /// Seeks the clip, or loads it paused at `seconds`.
    fn seek(
        &mut self,
        url: String,
        voice: bool,
        media: Media,
        seconds: f64,
        owner: Option<EntityId>,
        cx: &mut Context<Self>,
    ) {
        match self.track.as_mut().filter(|track| track.url == url) {
            Some(track) => {
                track.position = seconds;
                track.ended = false;
                if let Some(engine) = track.engine.as_mut() {
                    engine.seek(seconds);
                }
            }
            None => self.start(url, voice, media, false, seconds, owner, cx),
        }
        cx.notify();
    }

    fn cycle_rate(&mut self, url: &str, cx: &mut Context<Self>) {
        let rate = self.rates.get(url).copied().unwrap_or(1.);
        let next = VOICE_RATES
            .iter()
            .position(|r| *r == rate)
            .map_or(1., |i| VOICE_RATES[(i + 1) % VOICE_RATES.len()]);
        self.rates.insert(url.to_string(), next);
        if let Some(engine) = self
            .track
            .as_mut()
            .filter(|track| track.url == url)
            .and_then(|track| track.engine.as_mut())
        {
            engine.set_rate(next);
        }
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        url: String,
        voice: bool,
        media: Media,
        play: bool,
        position: f64,
        owner: Option<EntityId>,
        cx: &mut Context<Self>,
    ) {
        self.failed.remove(&url);
        let task_url = url.clone();
        let local = self.local.get(&url).cloned();
        let task = cx.spawn(async move |this, cx| {
            let url = task_url;
            let source = match (local, media.stream_url(&url)) {
                (Some(bytes), _) => Ok(Source::Bytes(bytes)),
                (None, Some(uri)) => Ok(Source::Uri(uri)),
                (None, None) => media
                    .fetch(&url)
                    .await
                    .map(Source::Bytes)
                    .map_err(|error| error.to_string()),
            };
            let engine = match source {
                Ok(source) => {
                    cx.background_executor()
                        .spawn(async move { Engine::new(source, Kind::Audio) })
                        .await
                }
                Err(error) => Err(error),
            };
            let loaded = this.update(cx, |this, cx| this.loaded(&url, engine, cx));
            if !matches!(loaded, Ok(true)) {
                return;
            }
            let Ok(follow) = this.update(cx, |_, cx| Self::follow(url, cx)) else {
                return;
            };
            follow.await;
        });
        // Dropping the previous track stops it.
        self.track = Some(Track {
            url,
            voice,
            owner,
            engine: None,
            ready: false,
            playing: play,
            ended: false,
            position,
            _task: task,
        });
    }

    fn loaded(
        &mut self,
        url: &str,
        engine: Result<Engine, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(track) = self.track.as_mut().filter(|track| track.url == url) else {
            return false;
        };
        cx.notify();
        match engine {
            Ok(mut engine) => {
                if let Some(rate) = self.rates.get(url) {
                    engine.set_rate(*rate);
                }
                if track.position > 0. {
                    engine.seek(track.position);
                }
                if track.playing && !engine.play() {
                    track.playing = false;
                    self.failed.insert(url.to_string());
                }
                track.engine = Some(engine);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "audio unavailable");
                self.failed.insert(url.to_string());
                self.track = None;
                false
            }
        }
    }

    /// Ticks until the clip stops playing, or is no longer current.
    fn follow(url: String, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if !matches!(this.update(cx, |this, cx| this.tick(&url, cx)), Ok(true)) {
                    break;
                }
            }
        })
    }

    /// Follows the clip; `false` once it is no longer the current one, or
    /// is paused, ended or failed with nothing more to read.
    fn tick(&mut self, url: &str, cx: &mut Context<Self>) -> bool {
        let Some(track) = self.track.as_mut().filter(|track| track.url == url) else {
            return false;
        };
        let Some(engine) = track.engine.as_mut() else {
            return true;
        };
        let before = (track.playing, track.position);
        let known = self.corrected.get(url).or(self.durations.get(url)).copied();
        for event in engine.poll() {
            match event {
                Event::Ready => track.ready = true,
                Event::Ended => {
                    track.playing = false;
                    engine.pause();
                    if track.voice {
                        // The voice card resets to the start.
                        engine.seek(0.);
                        track.position = 0.;
                    } else {
                        track.ended = true;
                        // Where it really ended; a container that claimed a
                        // little more is corrected (`onEnded`).
                        let ended = finite_duration(engine.position()).unwrap_or(track.position);
                        let gap = known.map(|known| known - ended);
                        if !self.corrected.contains_key(url)
                            && gap.is_some_and(|gap| gap > CORRECTION_MIN && gap <= CORRECTION_MAX)
                        {
                            self.corrected.insert(url.to_string(), ended);
                            track.position = ended;
                        } else {
                            track.position = known.unwrap_or(ended);
                        }
                    }
                }
                Event::Failed(_) => {
                    self.failed.insert(url.to_string());
                    self.track = None;
                    cx.notify();
                    return false;
                }
            }
        }
        if let Some(duration) = finite_duration(engine.duration()) {
            self.durations.insert(url.to_string(), duration);
        }
        if track.playing
            && let Some(position) = engine.position()
        {
            track.position = position;
        }
        if before != (track.playing, track.position) {
            cx.notify();
        }
        track.playing || !track.ready
    }
}

/// An inline audio player (`AudioAttachment`): play button, elapsed time,
/// seek bar with a hover time and the length, in a pill. The length is
/// read ahead when the sender gave none (`preload="metadata"`).
pub fn audio_attachment(
    id: impl Into<ElementId>,
    attachment: &Attachment,
    media: &Media,
    cx: &App,
) -> AnyElement {
    let (attachment, media) = (attachment.clone(), media.clone());
    let c = *cx.colors();
    let url = attachment.url.clone();
    let state = status(&url, attachment.duration, cx);
    if state.failed {
        return attachment_unavailable("Audio unavailable", Unavailable::Warning, &c)
            .into_any_element();
    }
    let hover = cx
        .try_global::<AudioGlobal>()
        .and_then(|audio| audio.0.read(cx).hover.clone())
        .filter(|(hovered, _)| *hovered == url)
        .map(|(_, at)| at);
    Lazy::new(id, move |window, cx| {
        let owner = Some(window.current_view());
        if state.duration.is_none() {
            probe(&url, &media, cx);
        }
        let label = |verb: &str| match &attachment.name {
            Some(name) => format!("{verb} {name}"),
            None => format!("{verb} audio"),
        };
        let (play_url, play_media) = (url.clone(), media.clone());
        let seek_to: Option<OnSeek> = state.duration.map(|duration| {
            let (url, media) = (url.clone(), media.clone());
            Rc::new(move |fraction: f32, _: &mut Window, cx: &mut App| {
                seek(&url, false, &media, fraction as f64 * duration, owner, cx)
            }) as OnSeek
        });
        let hover_url = url.clone();
        let on_hover: OnHover = Rc::new(move |at, _, cx| {
            let audio = audio(cx);
            audio.update(cx, |audio, cx| {
                let next = at.map(|at| (hover_url.clone(), at));
                let ours = audio.hover.as_ref().is_some_and(|(u, _)| *u == hover_url);
                if (next.is_some() || ours) && audio.hover != next {
                    audio.hover = next;
                    cx.notify();
                }
            });
        });
        let progress = progress(&state);
        div()
            .id("audio")
            .flex()
            .items_center()
            .gap(px(8.))
            .w(px(384.))
            .max_w_full()
            .mt(px(8.))
            .px(px(8.))
            .py(px(4.))
            .border_1()
            .border_color(c.border_standard)
            .rounded(px(32.))
            .bg(c.surface_inset)
            .text_color(c.text_standard)
            .child(
                IconButton::new(
                    "play",
                    if state.playing {
                        IconName::PauseFilled
                    } else {
                        IconName::PlayFilled
                    },
                )
                .size(IconButtonSize::Sm)
                .tooltip(label(if state.playing { "Pause" } else { "Play" }))
                .on_click(move |_, _, cx| toggle(&play_url, false, &play_media, owner, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .text_caption()
                    .font_features(tabular())
                    .child(video_time(state.position)),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w(px(20.))
                    .h(px(32.))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .when(state.duration.is_none(), |this| this.opacity(0.4))
                            .child(track_line(progress, c.border_prominent, c.text_standard)),
                    )
                    .child(seek_area_hover("seek", seek_to, on_hover))
                    .when_some(hover.zip(state.duration), |d, (at, duration)| {
                        d.child(hover_time(at, duration, &c))
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_caption()
                    .font_features(tabular())
                    .text_color(c.text_subtle)
                    .child(state.duration.map_or("—:—".into(), video_time)),
            )
            .into_any_element()
    })
    .into_any_element()
}

/// The voice note card (`VoiceNoteCard`): play button, waveform scrubber,
/// and the length, which turns into the speed button on hover.
pub fn voice_note(
    id: impl Into<ElementId>,
    attachment: &Attachment,
    media: &Media,
    cx: &App,
) -> AnyElement {
    voice_card(id.into(), attachment, media, false, cx)
}

/// A recorded note in the composer (`data-voice-draft`): it plays from
/// memory under `attachment.url` (see [`set_local`]), always shows its
/// length and has no speed button.
pub fn voice_draft(
    id: impl Into<ElementId>,
    attachment: &Attachment,
    media: &Media,
    cx: &App,
) -> AnyElement {
    voice_card(id.into(), attachment, media, true, cx)
}

/// `floor((width + 2) / 5)` bars of 3px with 2px gaps, 1 to 64.
fn bar_count(width: f32) -> usize {
    (((width + 2.) / 5.).floor() as usize).clamp(1, MAX_BARS)
}

fn voice_card(
    id: ElementId,
    attachment: &Attachment,
    media: &Media,
    draft: bool,
    cx: &App,
) -> AnyElement {
    let c = *cx.colors();
    let url = attachment.url.clone();
    let state = status(&url, attachment.duration, cx);
    let duration = state.duration.unwrap_or(0.);
    let width = cx
        .try_global::<AudioGlobal>()
        .and_then(|audio| audio.0.read(cx).widths.get(&id).copied());
    let heights = bar_heights(
        attachment.waveform.as_deref(),
        width.map_or(DEFAULT_BARS, bar_count),
    );
    let progress = progress(&state);
    let media = media.clone();
    let key = id.clone();
    Lazy::new(id, move |window, _| {
        let owner = Some(window.current_view());
        let (play_url, play_media) = (url.clone(), media.clone());
        // Seeking needs the clip's metadata, read on the first play.
        let seek_to: Option<OnSeek> = (duration > 0. && (state.measured || draft)).then(|| {
            let (url, media) = (url.clone(), media.clone());
            Rc::new(move |fraction: f32, _: &mut Window, cx: &mut App| {
                seek(&url, true, &media, fraction as f64 * duration, owner, cx)
            }) as OnSeek
        });
        let bars = |color: Hsla| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .gap(px(2.))
                .children(heights.iter().map(move |height| {
                    div()
                        .flex_none()
                        .w(px(3.))
                        .h(px(*height))
                        .rounded_full()
                        .bg(color)
                }))
        };
        let label = if state.failed {
            "Retry voice note"
        } else if state.loading {
            "Cancel loading voice note"
        } else if state.playing {
            "Pause voice note"
        } else {
            "Play voice note"
        };
        let glyph = if state.failed {
            Icon::new(IconName::WarningCircle)
                .size(18.)
                .into_any_element()
        } else if state.loading {
            svg()
                .path(IconName::CircleNotch.path())
                .flex_none()
                .size(px(18.))
                .text_color(c.text_inverse)
                .with_animation(
                    "voice-spinner",
                    Animation::new(SPIN).repeat(),
                    |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
                )
                .into_any_element()
        } else if state.playing {
            Icon::new(IconName::Pause).size(20.).into_any_element()
        } else {
            Icon::new(IconName::Play).size(20.).into_any_element()
        };
        let rate_url = url.clone();
        let rate = state.rate;
        let measure_key = key.clone();
        let inner_width = width.unwrap_or(210.);
        div()
            .id("voice-note")
            .group("voice-note")
            .relative()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .w(px(336.))
            .max_w_full()
            .my(px(4.))
            .px(px(10.))
            .py(px(8.))
            .border_1()
            .border_color(c.border_standard)
            .rounded(px(16.))
            .bg(c.surface_inset)
            .hover(|s| {
                s.border_color(c.border_prominent)
                    .bg(c.affordance_panel_hover)
            })
            .text_color(c.text_standard)
            .child(
                IconButton::new("play", IconName::Play)
                    .variant(ButtonVariant::Prominent)
                    .size(IconButtonSize::Sm)
                    .child(glyph)
                    .tooltip(label)
                    .on_click(move |_, _, cx| toggle(&play_url, true, &play_media, owner, cx)),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h(px(24.))
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let width = f32::from(bounds.size.width);
                                let audio = audio(cx);
                                let known = audio.read(cx).widths.get(&measure_key).copied();
                                if known != Some(width) {
                                    let key = measure_key.clone();
                                    audio.update(cx, |audio, cx| {
                                        audio.widths.insert(key, width);
                                        cx.notify();
                                    });
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0()
                        .size_full(),
                    )
                    .child(bars(c.text_unavailable))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .bottom_0()
                            .w(relative(progress))
                            .overflow_hidden()
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .w(px(inner_width))
                                    .child(bars(c.text_standard)),
                            ),
                    )
                    .child(seek_area("seek", seek_to)),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(48.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_caption()
                            .font_features(tabular())
                            .text_color(c.text_subtle)
                            .when(!draft, |d| d.group_hover("voice-note", |s| s.opacity(0.)))
                            .child(media_time(if state.playing {
                                (duration - state.position).max(0.)
                            } else {
                                duration
                            })),
                    )
                    .when(!draft, |d| {
                        d.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .opacity(0.)
                                .group_hover("voice-note", |s| s.opacity(1.))
                                .child(
                                    Button::new(
                                        "speed",
                                        if rate == 0.5 {
                                            ".5×".to_string()
                                        } else {
                                            format!("{rate}×")
                                        },
                                    )
                                    .variant(ButtonVariant::Prominent)
                                    .size(ButtonSize::Xs)
                                    .full_width()
                                    .tooltip(format!(
                                        "Playback speed {rate} times; change to {} times",
                                        next_rate(rate)
                                    ))
                                    .on_click(
                                        move |_, _, cx| {
                                            audio(cx).update(cx, |audio, cx| {
                                                audio.cycle_rate(&rate_url, cx)
                                            })
                                        },
                                    ),
                                ),
                        )
                    }),
            )
            .when(state.failed, |this| {
                this.child(
                    div()
                        .w_full()
                        .text_caption()
                        .text_color(c.text_danger)
                        .child("Audio unavailable. Retry playback."),
                )
            })
            .into_any_element()
    })
    .into_any_element()
}

/// `nextVoiceNotePlaybackRate`.
fn next_rate(rate: f64) -> f64 {
    VOICE_RATES
        .iter()
        .position(|r| *r == rate)
        .map_or(1., |i| VOICE_RATES[(i + 1) % VOICE_RATES.len()])
}

fn progress(state: &Status) -> f32 {
    match state.duration {
        Some(duration) if duration > 0. => (state.position / duration).clamp(0., 1.) as f32,
        _ => 0.,
    }
}

/// A 2px track with its played part and a 2×16px thumb, centred in a
/// `relative` parent.
pub(super) fn track_line(progress: f32, rest: Hsla, played: Hsla) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(relative(0.5))
                .mt(px(-1.))
                .h(px(2.))
                .bg(rest),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .w(relative(progress))
                .top(relative(0.5))
                .mt(px(-1.))
                .h(px(2.))
                .bg(played),
        )
        .child(
            div()
                .absolute()
                .left(relative(progress))
                .ml(px(-1.))
                .top(relative(0.5))
                .mt(px(-8.))
                .w(px(2.))
                .h(px(16.))
                .rounded_full()
                .bg(played),
        )
}

/// Bar heights in pixels (`waveformPeaks` then `voiceNoteBarHeight`). The
/// signed waveform is 0..=100 levels; without one every bar is flat.
fn bar_heights(waveform: Option<&[u8]>, count: usize) -> Vec<f32> {
    let Some(samples) = waveform.filter(|w| !w.is_empty()) else {
        return vec![3.; count];
    };
    let samples = &samples[..samples.len().min(100)];
    let peaks: Vec<f32> = (0..count)
        .map(|index| {
            let start = index * samples.len() / count;
            let end = ((index + 1) * samples.len() / count).max(start + 1);
            samples[start..end.min(samples.len())]
                .iter()
                .map(|s| *s as f32)
                .fold(0., f32::max)
        })
        .collect();
    let maximum = peaks.iter().copied().fold(0.001, f32::max);
    peaks
        .into_iter()
        .map(|peak| {
            let level = (peak / maximum).clamp(0.12, 1.);
            let audible = ((level - 0.16) / (1. - 0.16)).max(0.);
            3. + (audible * 17.).round()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn scales_waveform_bars_like_the_reference() {
        assert_eq!(bar_heights(None, 4), vec![3.; 4]);
        assert_eq!(
            bar_heights(Some(&[0, 50, 100, 25]), 4),
            vec![3., 10., 20., 5.]
        );
        // Fewer samples than bars repeat them; more are max-pooled.
        assert_eq!(bar_heights(Some(&[100]), 3), vec![20.; 3]);
        assert_eq!(bar_heights(Some(&[10, 100, 0, 0]), 2), vec![20., 3.]);
    }

    #[test]
    fn fits_bars_to_the_track() {
        assert_eq!(bar_count(208.), 42);
        assert_eq!(bar_count(0.), 1);
        assert_eq!(bar_count(1_000.), MAX_BARS);
    }

    #[test]
    fn cycles_voice_speeds() {
        assert_eq!(next_rate(1.), 1.5);
        assert_eq!(next_rate(0.5), 1.);
        assert_eq!(next_rate(3.), 1.);
    }
}
