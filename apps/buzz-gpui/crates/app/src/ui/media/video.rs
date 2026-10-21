//! Video: the inline preview (`MediaAttachment.tsx`) and the player in the
//! media viewer (`VideoPlayer.tsx`). One [`VideoPlayer`] serves both. The
//! inline one shows its poster and loads only when played, with the media
//! glass controls over the picture; the viewer's autoplays, with gestures
//! on the picture and the controls in a bar below it.

use super::engine::{self, Engine, Event, Kind, Source};
use super::preview::{Unavailable, attachment_unavailable};
use super::{Lazy, Remote, render_image, tabular};
use crate::community::AppState;
use buzz_relay_client::fold::Attachment;
use buzz_relay_client::{EventId, Media};
use buzz_ui::{
    ActiveTheme, Appearance, ButtonVariant, Icon, IconButton, IconButtonSize, IconName, ThemeScope,
    TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Frame polling while a player is loaded and busy.
const TICK: Duration = Duration::from_millis(16);
/// A paused player keeps polling this long after it was last drawn or
/// touched, for the frame a seek decodes.
const SETTLE: Duration = Duration::from_secs(1);
/// Speeds the menu offers, fastest first (`speeds`).
pub(super) const SPEEDS: [f64; 8] = [2., 1.75, 1.5, 1.25, 1., 0.75, 0.5, 0.25];
/// The device preference holding the chosen speed (`speedKey`).
const SPEED_KEY: &str = "buzz.video.playback-speed";
/// Arrow keys and double clicks skip this far (`seekVideoBy`).
pub const SKIP_SECONDS: f64 = 10.;
/// Pointer stillness before playing controls hide (`useMediaControls`).
pub const IDLE: Duration = Duration::from_millis(2200);
/// How long a gesture badge shows (`video-feedback-*`).
const FEEDBACK: Duration = Duration::from_millis(650);
/// A click toggles playback only once no double click follows.
const CLICK_DELAY: Duration = Duration::from_millis(250);
/// Holding the right half this long plays at 2x until release.
const HOLD_DELAY: Duration = Duration::from_millis(350);
/// Controls fade in and out over this (`160ms ease-out`).
const CONTROLS_FADE: Duration = Duration::from_millis(160);
/// The longest length believed (`MAX_ATTACHMENT_DURATION_SECONDS`).
pub const MAX_DURATION: f64 = 86_400.;
/// Viewer players narrower than this put the timeline on its own row
/// (`@container video-player (max-width: 25rem)`).
const NARROW_PLAYER: f32 = 400.;
/// Inline controls drop the length and their vertical padding at this
/// preview width (`@container media-preview (max-width: 22rem)`).
const NARROW_PREVIEW: f32 = 352.;

/// A length worth showing: finite, positive and at most a day.
pub fn finite_duration(seconds: Option<f64>) -> Option<f64> {
    seconds.filter(|s| s.is_finite() && *s > 0. && *s <= MAX_DURATION)
}

/// The chosen speed, shared by every player (`buzz:video-playback-speed`).
struct Speed(f64);

impl Global for Speed {}

/// The saved playback speed (`savedSpeed`), 1 when unset or unknown.
pub fn playback_speed(cx: &mut App) -> f64 {
    if let Some(speed) = cx.try_global::<Speed>() {
        return speed.0;
    }
    let saved = cx
        .try_global::<AppState>()
        .and_then(|state| state.store.device_pref(SPEED_KEY).ok().flatten())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| SPEEDS.contains(value))
        .unwrap_or(1.);
    cx.set_global(Speed(saved));
    saved
}

/// Saves `speed` and hands it to every open player.
pub(super) fn set_playback_speed(speed: f64, cx: &mut App) {
    cx.set_global(Speed(speed));
    if let Some(state) = cx.try_global::<AppState>()
        && let Err(error) = state.store.set_device_pref(SPEED_KEY, &speed.to_string())
    {
        tracing::warn!(%error, "could not save the playback speed");
    }
    cx.refresh_windows();
}

fn current_speed(cx: &App) -> f64 {
    cx.try_global::<Speed>().map_or(1., |speed| speed.0)
}

/// Where a player reads its video.
#[derive(Clone)]
pub enum Clip {
    /// An attachment, streamed from the relay or fetched whole.
    Remote {
        attachment: Attachment,
        media: Media,
    },
    /// A file on this computer: a composer draft.
    Local(PathBuf),
}

impl Clip {
    /// A signed poster the sender attached.
    fn poster(&self) -> Option<ImageSource> {
        match self {
            Self::Remote { attachment, media } => attachment
                .preview_url
                .as_ref()
                .map(|url| super::image(url, media)),
            Self::Local(_) => None,
        }
    }

    /// The stream for HTTPS relay media; the whole body for anything else.
    async fn source(self) -> Result<Source, String> {
        match self {
            Self::Remote { attachment, media } => match media.stream_url(&attachment.url) {
                Some(uri) => Ok(Source::Uri(uri)),
                None => media
                    .fetch(&attachment.url)
                    .await
                    .map(Source::Bytes)
                    .map_err(|error| error.to_string()),
            },
            Self::Local(path) => Source::file(&path),
        }
    }
}

/// A video's first frame and length, decoded once per URL for previews
/// without a signed poster or size. Streamed media is probed with ranged
/// reads, so only the start (and the index) is fetched.
struct Probed {
    poster: Option<Arc<RenderImage>>,
    size: Option<Size<f32>>,
    duration: Option<f64>,
}

enum VideoProbe {}

impl Asset for VideoProbe {
    type Source = Remote;
    type Output = Result<Arc<Probed>, SharedString>;

    #[allow(clippy::manual_async_fn)]
    fn load(source: Remote, _: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        async move {
            let source = match source.media.stream_url(&source.url) {
                Some(uri) => Source::Uri(uri),
                None => Source::Bytes(
                    source
                        .media
                        .fetch(&source.url)
                        .await
                        .map_err(|error| SharedString::from(error.to_string()))?,
                ),
            };
            let (sender, receiver) = futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = sender.send(engine::probe_source(source, Kind::Poster));
            });
            let probe = receiver
                .await
                .map_err(|_| SharedString::from("The video decoder stopped"))??;
            let size = probe
                .frame
                .as_ref()
                .map(|frame| size(frame.width as f32, frame.height as f32));
            Ok(Arc::new(Probed {
                poster: probe
                    .frame
                    .and_then(|frame| render_image(frame.width, frame.height, frame.bgra)),
                size,
                duration: probe.duration,
            }))
        }
    }
}

/// Opens the viewer from an inline preview at a time, handing it the
/// preview so the viewer's position comes back to it.
pub type OpenReview = Rc<dyn Fn(f64, WeakEntity<VideoPlayer>, &mut Window, &mut App)>;

/// The inline video preview (`MediaAttachment`, `data-video-preview`):
/// `min(24rem, 100%, ratio × 11.25rem)` wide, 16px corners, a centred play
/// button, media glass controls and an expand button that opens the
/// viewer at the current time. `seek` asks it to play from a time (a
/// thread's time chip); a new request number seeks again.
pub fn video_preview(
    id: impl Into<ElementId>,
    attachment: &Attachment,
    media: &Media,
    seek: Option<(f64, u64)>,
    open: OpenReview,
) -> impl IntoElement {
    let attachment = attachment.clone();
    let media = media.clone();
    Lazy::new(id, move |window, cx| {
        let c = *cx.colors();
        let clip = Clip::Remote {
            attachment: attachment.clone(),
            media: media.clone(),
        };
        // A chip seek made before this preview existed is not replayed.
        let player = window.use_keyed_state("player", cx, |_, cx| {
            let mut player = VideoPlayer::new(clip, Mode::Inline, 0., cx);
            player.applied_seek = seek.map(|(_, request)| request);
            player
        });
        let dims = attachment
            .dimensions
            .map(|d| size(d.width as f32, d.height as f32));
        let probe =
            (attachment.preview_url.is_none() || dims.is_none() || attachment.duration.is_none())
                .then(|| {
                    let remote = Remote {
                        url: attachment.url.clone().into(),
                        fallback: None,
                        media: media.clone(),
                    };
                    window.use_asset::<VideoProbe>(&remote, cx)
                })
                .flatten();
        let (probed, probe_failed) = match probe {
            Some(Ok(probed)) => (Some(probed), false),
            Some(Err(_)) => (None, true),
            None => (None, false),
        };
        let measured = player.read(cx).natural;
        let natural = dims.or(probed.as_ref().and_then(|p| p.size)).or(measured);
        let duration = finite_duration(attachment.duration)
            .or(probed.as_ref().and_then(|p| finite_duration(p.duration)));
        let poster = probed.as_ref().and_then(|p| p.poster.clone());
        let failed = player.update(cx, |player, _| {
            player.hint_duration = duration;
            player.captured = poster.map(ImageSource::from);
            player.open = Some(open.clone());
            if let Some((seconds, request)) = seek {
                player.request_seek(seconds, request);
            }
            player.failed()
        });
        // A failed probe means the file cannot play either.
        if failed || (probe_failed && attachment.preview_url.is_none()) {
            return attachment_unavailable("Video unavailable", Unavailable::Note, &c)
                .into_any_element();
        }
        let ratio = natural
            .filter(|s| s.width > 0. && s.height > 0.)
            .map_or(16. / 9., |s| s.width / s.height);
        let width = (ratio * 180.).min(384.);
        div()
            .relative()
            .w(px(width))
            .h(px(width / ratio))
            .max_w_full()
            .max_h(px(256.))
            .my(px(4.))
            .rounded(px(16.))
            .overflow_hidden()
            .bg(c.surface_inverse)
            .child(super::review::track_tile(&attachment.url))
            // The pixels and their controls are always dark (`.mediaPreviewPixels.dark`).
            .child(ThemeScope::new(Appearance::Dark, move |_, _| player))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(16.))
                    .border_1()
                    .border_color(c.border_image),
            )
            .into_any_element()
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// In a message row.
    Inline,
    /// In the media viewer, or a draft's preview dialog.
    Review,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Inline, before the first play: only the poster.
    Idle,
    Loading,
    Ready,
    Failed,
}

/// Which hover popup of the controls is open.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Popup {
    Volume,
    Speed,
}

/// A gesture's transient badge (`useVideoGestures` feedback).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Gesture {
    Play,
    Pause,
    Forward,
    Backward,
    /// Held while the right half is pressed.
    Speed,
}

/// A timecoded comment on the timeline (`VideoMarker`).
#[derive(Clone)]
pub struct Marker {
    pub id: EventId,
    pub seconds: f64,
    pub label: String,
    pub author: SharedString,
    pub picture: Option<ImageSource>,
    pub text: String,
}

/// Called when a marker is clicked, after the player seeks to it.
pub type OnMarker = Rc<dyn Fn(EventId, &mut Window, &mut App)>;
/// Extra chrome in the viewer's control bar: the reactions.
pub type Chrome = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// An opacity easing toward shown or hidden.
pub(super) struct Fade {
    shown: bool,
    from: f32,
    changed: Option<Instant>,
}

impl Fade {
    pub(super) fn new(shown: bool) -> Self {
        Self {
            shown,
            from: if shown { 1. } else { 0. },
            changed: None,
        }
    }

    /// The current opacity, and whether it still moves.
    pub(super) fn level(&mut self, shown: bool, duration: Duration, instant: bool) -> (f32, bool) {
        if shown != self.shown {
            self.from = self.value(duration);
            self.shown = shown;
            self.changed = Some(Instant::now());
        }
        if instant {
            self.changed = None;
        }
        let value = self.value(duration);
        let target = if self.shown { 1. } else { 0. };
        (value, (value - target).abs() > f32::EPSILON)
    }

    fn value(&self, duration: Duration) -> f32 {
        let target = if self.shown { 1. } else { 0. };
        let Some(changed) = self.changed else {
            return target;
        };
        let t = (changed.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.);
        let eased = 1. - (1. - t) * (1. - t);
        self.from + (target - self.from) * eased
    }
}

/// A click waiting to see whether a double click follows.
struct PendingClick {
    at: Instant,
    was_paused: bool,
    committed: bool,
}

/// A press on the right half that may become a 2x hold.
struct Hold {
    at: Instant,
    /// Set once the hold took effect: the playing state to restore.
    held: Option<bool>,
}

pub struct VideoPlayer {
    pub(super) clip: Clip,
    pub(super) mode: Mode,
    engine: Option<Engine>,
    state: State,
    started: bool,
    pub(super) playing: bool,
    ended: bool,
    pub(super) position: f64,
    pub(super) duration: Option<f64>,
    /// The sender's or the probe's length, until the engine knows.
    hint_duration: Option<f64>,
    pub(super) volume: f32,
    pub(super) muted: bool,
    /// The volume slider animates only after a mute click.
    pub(super) volume_motion: bool,
    pub(super) popup: Option<Popup>,
    pub(super) popup_hovered: bool,
    /// "Playback paused. Press play to try again."
    pub(super) notice: Option<SharedString>,
    frame: Option<Arc<RenderImage>>,
    /// The size of the decoded frames.
    natural: Option<Size<f32>>,
    /// Frames replaced since the last draw, to free from the atlas.
    retired: Vec<Arc<RenderImage>>,
    /// The window it draws in, whose atlas frees its frames when it goes.
    window: Option<AnyWindowHandle>,
    /// A first frame decoded for the inline poster.
    captured: Option<ImageSource>,
    /// The player's bounds as last drawn.
    pub(super) bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The timeline position under the mouse.
    pub(super) hover: Option<f32>,
    /// Last pointer activity over the player; `None` once it left.
    activity: Option<Instant>,
    pub(super) feedback: Option<(Gesture, Instant)>,
    click: Option<PendingClick>,
    hold: Option<Hold>,
    /// The time to start from once loaded.
    start_at: f64,
    applied_seek: Option<u64>,
    pub(super) markers: Vec<Marker>,
    pub(super) on_marker: Option<OnMarker>,
    pub(super) chrome: Option<Chrome>,
    /// The inline preview's opener, from its row.
    open: Option<OpenReview>,
    pub(super) timeline_focus: FocusHandle,
    pub(super) volume_focus: FocusHandle,
    pub(super) controls_fade: Fade,
    center_fade: Fade,
    expand_fade: Fade,
    /// When the player was last drawn or touched.
    woke: Instant,
    /// Loads the pipeline.
    _task: Option<Task<()>>,
    /// Polls the pipeline and the gesture timers while there is something
    /// to follow; drawing restarts it.
    ticks: Option<Task<()>>,
    _release: Subscription,
}

impl VideoPlayer {
    /// A player at `start` seconds. The viewer's loads and autoplays now;
    /// an inline one waits for play.
    pub fn new(clip: Clip, mode: Mode, start: f64, cx: &mut Context<Self>) -> Self {
        playback_speed(cx);
        let mut player = Self {
            clip,
            mode,
            engine: None,
            state: State::Idle,
            started: false,
            playing: false,
            ended: false,
            position: start.max(0.),
            duration: None,
            hint_duration: None,
            volume: 1.,
            muted: false,
            volume_motion: false,
            popup: None,
            popup_hovered: false,
            notice: None,
            frame: None,
            natural: None,
            retired: Vec::new(),
            window: None,
            captured: None,
            bounds: Rc::default(),
            hover: None,
            activity: Some(Instant::now()),
            feedback: None,
            click: None,
            hold: None,
            start_at: start.max(0.),
            applied_seek: None,
            markers: Vec::new(),
            on_marker: None,
            chrome: None,
            open: None,
            timeline_focus: cx.focus_handle(),
            volume_focus: cx.focus_handle(),
            controls_fade: Fade::new(mode == Mode::Review),
            center_fade: Fade::new(true),
            expand_fade: Fade::new(false),
            woke: Instant::now(),
            _task: None,
            ticks: None,
            _release: cx.on_release(Self::release_frames),
        };
        if mode == Mode::Inline {
            player.activity = None;
        } else {
            // `autoPlay`.
            player.playing = true;
            player.load(cx);
        }
        player
    }

    pub(super) fn failed(&self) -> bool {
        self.state == State::Failed
    }

    pub(super) fn loading(&self) -> bool {
        matches!(self.state, State::Loading) && self.frame.is_none()
    }

    /// Starts the pipeline; it plays once loaded if `playing` is set.
    fn load(&mut self, cx: &mut Context<Self>) {
        if self.state != State::Idle {
            return;
        }
        self.state = State::Loading;
        let clip = self.clip.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let engine = match clip.source().await {
                Ok(source) => {
                    cx.background_executor()
                        .spawn(async move { Engine::new(source, Kind::Video) })
                        .await
                }
                Err(error) => Err(error),
            };
            let _ = this.update(cx, |this, cx| {
                if this.loaded(engine, cx) {
                    this.wake(cx);
                }
            });
        }));
    }

    /// Something to poll for: playback, loading, a pending gesture, the
    /// controls about to hide, or a recent draw that may have sought.
    fn busy(&self) -> bool {
        self.engine.is_some()
            && (self.playing
                || self.state != State::Ready
                || self.click.as_ref().is_some_and(|click| !click.committed)
                || self.hold.is_some()
                || self.feedback.is_some()
                || self
                    .activity
                    .is_some_and(|at| at.elapsed() < IDLE + TICK * 2)
                || self.woke.elapsed() < SETTLE)
    }

    /// Starts polling again if it stopped and there is something to poll.
    fn wake(&mut self, cx: &mut Context<Self>) {
        self.woke = Instant::now();
        if self.ticks.is_some() || !self.busy() {
            return;
        }
        self.ticks = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let busy = this.update(cx, |this, cx| {
                    this.tick(cx);
                    let busy = this.busy();
                    if !busy {
                        this.ticks = None;
                    }
                    busy
                });
                if !matches!(busy, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn loaded(&mut self, engine: Result<Engine, String>, cx: &mut Context<Self>) -> bool {
        cx.notify();
        match engine {
            Ok(mut engine) => {
                engine.set_volume(self.volume as f64);
                engine.set_muted(self.muted);
                engine.set_rate(current_speed(cx));
                if self.start_at > 0. {
                    engine.seek(self.start_at);
                }
                if self.playing && !engine.play() {
                    self.refused();
                }
                self.engine = Some(engine);
                true
            }
            Err(error) => {
                tracing::warn!(%error, "video unavailable");
                self.state = State::Failed;
                false
            }
        }
    }

    fn refused(&mut self) {
        self.playing = false;
        self.notice = Some("Playback paused. Press play to try again.".into());
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = self.timers(cx);
        let held = self.hold.as_ref().is_some_and(|hold| hold.held.is_some());
        let speed = if held { 2. } else { current_speed(cx) };
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        if engine.rate() != speed {
            engine.set_rate(speed);
            changed = true;
        }
        for event in engine.poll() {
            changed = true;
            match event {
                Event::Ready => self.state = State::Ready,
                Event::Ended => {
                    self.playing = false;
                    self.ended = true;
                    engine.pause();
                    if let Some(duration) = self.duration {
                        self.position = duration;
                    }
                }
                Event::Failed(_) => {
                    self.state = State::Failed;
                    self.engine = None;
                    cx.notify();
                    return;
                }
            }
        }
        if let Some(frame) = engine.take_frame()
            && let Some(image) = render_image(frame.width, frame.height, frame.bgra)
        {
            self.natural = Some(size(frame.width as f32, frame.height as f32));
            self.retired.extend(self.frame.replace(image));
            changed = true;
        }
        if self.duration.is_none() {
            self.duration = finite_duration(engine.duration());
        }
        if self.playing
            && let Some(position) = engine.position()
            && (position - self.position).abs() > 0.01
            && !self.ended
        {
            self.position = position;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Gesture and idle deadlines; `true` when something changed.
    fn timers(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        if let Some(click) = self.click.as_mut()
            && !click.committed
            && click.at.elapsed() >= CLICK_DELAY
        {
            click.committed = true;
            self.toggle(cx);
            changed = true;
        }
        if let Some(hold) = self.hold.as_mut()
            && hold.held.is_none()
            && hold.at.elapsed() >= HOLD_DELAY
        {
            hold.held = Some(self.playing);
            self.click = None;
            self.feedback = Some((Gesture::Speed, Instant::now()));
            if !self.playing {
                self.play(cx);
            }
            changed = true;
        }
        if self
            .feedback
            .is_some_and(|(kind, at)| kind != Gesture::Speed && at.elapsed() >= FEEDBACK)
        {
            self.feedback = None;
            changed = true;
        }
        if self
            .activity
            .is_some_and(|at| at.elapsed() >= IDLE && at.elapsed() < IDLE + TICK * 2)
        {
            changed = true;
        }
        changed
    }

    /// Seconds into the video, for comments at the current frame.
    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn duration(&self) -> Option<f64> {
        self.duration.or(self.hint_duration)
    }

    pub fn play(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        self.started = true;
        self.playing = true;
        match self.engine.as_mut() {
            None => self.load(cx),
            Some(engine) => {
                if self.ended {
                    engine.seek(0.);
                    self.position = 0.;
                    self.ended = false;
                }
                if !engine.play() {
                    self.refused();
                }
            }
        }
        cx.notify();
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.playing = false;
        if let Some(engine) = &self.engine {
            engine.pause();
        }
        cx.notify();
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.playing {
            self.pause(cx);
        } else {
            self.play(cx);
        }
    }

    pub fn seek_to(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let seconds = match self.duration() {
            Some(duration) => seconds.clamp(0., duration),
            None => seconds.max(0.),
        };
        match self.engine.as_mut() {
            Some(engine) => engine.seek(seconds),
            None => self.start_at = seconds,
        }
        self.position = seconds;
        self.ended = false;
        cx.notify();
    }

    pub fn seek_by(&mut self, delta: f64, cx: &mut Context<Self>) {
        self.seek_to(self.position + delta, cx);
    }

    /// Seeks and plays (a time chip, a marker click).
    pub fn play_from(&mut self, seconds: f64, cx: &mut Context<Self>) {
        self.seek_to(seconds, cx);
        self.play(cx);
    }

    /// Plays from a thread's time chip, once per request number
    /// (`useVideoPosition`).
    fn request_seek(&mut self, seconds: f64, request: u64) {
        if self.applied_seek == Some(request) {
            return;
        }
        self.applied_seek = Some(request);
        self.start_at = seconds.max(0.);
        self.position = self.start_at;
        self.ended = false;
        self.started = true;
        self.playing = true;
        self.notice = None;
        if let Some(engine) = self.engine.as_mut() {
            engine.seek(self.start_at);
            if !engine.play() {
                self.refused();
            }
        }
        // Loading needs the context; the next tick of the row starts it.
    }

    /// The viewer's position, handed back to the inline preview it came
    /// from (`onTime`).
    pub fn follow(&mut self, seconds: f64, cx: &mut Context<Self>) {
        if (seconds - self.position).abs() < 0.01 {
            return;
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.seek(seconds);
        } else {
            self.start_at = seconds;
        }
        self.position = seconds;
        self.ended = false;
        cx.notify();
    }

    pub(super) fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0., 1.);
        self.muted = false;
        self.volume_motion = false;
        if let Some(engine) = &self.engine {
            engine.set_volume(self.volume as f64);
            engine.set_muted(false);
        }
        cx.notify();
    }

    pub(super) fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        let silent = self.muted || self.volume == 0.;
        self.muted = !silent;
        if !self.muted && self.volume == 0. {
            self.volume = 1.;
        }
        self.volume_motion = true;
        if let Some(engine) = &self.engine {
            engine.set_volume(self.volume as f64);
            engine.set_muted(self.muted);
        }
        cx.notify();
    }

    pub(super) fn set_speed(&mut self, speed: f64, cx: &mut Context<Self>) {
        self.popup = None;
        set_playback_speed(speed, cx);
        if let Some(engine) = self.engine.as_mut() {
            engine.set_rate(speed);
        }
        cx.notify();
    }

    /// The pointer moved over the player, or pressed or scrolled in it.
    pub(super) fn active(&mut self, cx: &mut Context<Self>) {
        let was_idle = self.idle();
        self.activity = Some(Instant::now());
        self.wake(cx);
        if was_idle {
            cx.notify();
        }
    }

    fn left(&mut self, cx: &mut Context<Self>) {
        self.activity = None;
        self.hover = None;
        self.end_hold(cx);
        cx.notify();
    }

    /// No pointer over the player, or still for [`IDLE`].
    fn idle(&self) -> bool {
        self.activity.is_none_or(|at| at.elapsed() >= IDLE)
    }

    /// Shows or schedules closing a hover popup (`openOnHover`, 100ms).
    pub(super) fn hover_popup(&mut self, popup: Popup, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.popup = Some(popup);
            self.popup_hovered = true;
            cx.notify();
            return;
        }
        self.popup_hovered = false;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.popup_hovered && this.popup == Some(popup) {
                    this.popup = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Frees the frames still held when the player goes; only drawing
    /// freed them before, so the last one leaked.
    fn release_frames(&mut self, cx: &mut App) {
        let frames = self.take_frames();
        if let Some(window) = self.window
            && !frames.is_empty()
        {
            let _ = window.update(cx, |_, window, _| {
                for image in frames {
                    let _ = window.drop_image(image);
                }
            });
        }
    }

    /// Every frame still held, for a closing viewer to free.
    pub fn take_frames(&mut self) -> Vec<Arc<RenderImage>> {
        let mut frames = std::mem::take(&mut self.retired);
        frames.extend(self.frame.take());
        frames
    }

    /// The picture's aspect ratio once known.
    pub fn natural(&self) -> Option<Size<f32>> {
        self.natural
    }

    /// The last decoded frame, for the viewer's shared-element motion.
    pub fn frame(&self) -> Option<Arc<RenderImage>> {
        self.frame.clone()
    }

    pub fn set_markers(&mut self, markers: Vec<Marker>, on_marker: OnMarker) {
        self.markers = markers;
        self.on_marker = Some(on_marker);
    }

    pub fn set_chrome(&mut self, chrome: Option<Chrome>) {
        self.chrome = chrome;
    }

    /// A press on the picture: the right half may become a 2x hold.
    fn press(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.click = self.click.take().filter(|click| !click.committed);
        self.end_hold(cx);
        let bounds = self.bounds.get();
        if position.x >= bounds.center().x {
            self.hold = Some(Hold {
                at: Instant::now(),
                held: None,
            });
        }
        self.wake(cx);
    }

    fn end_hold(&mut self, cx: &mut Context<Self>) {
        let Some(hold) = self.hold.take() else {
            return;
        };
        if let Some(was_playing) = hold.held {
            if self
                .feedback
                .is_some_and(|(kind, _)| kind == Gesture::Speed)
            {
                self.feedback = None;
            }
            if let Some(engine) = self.engine.as_mut() {
                engine.set_rate(current_speed(cx));
            }
            if !was_playing {
                self.pause(cx);
            }
            // The release that ends a hold is not a click.
            self.click = Some(PendingClick {
                at: Instant::now(),
                was_paused: true,
                committed: true,
            });
            cx.notify();
        }
    }

    /// A click on the picture: the badge shows now, playback toggles once
    /// no double click follows; a double click seeks by the half it hit.
    fn picture_click(&mut self, event: &ClickEvent, cx: &mut Context<Self>) {
        let held = self
            .click
            .as_ref()
            .is_some_and(|click| click.committed && click.at.elapsed() < CLICK_DELAY);
        if event.click_count() >= 2 {
            let pending = self.click.take();
            if held && pending.as_ref().is_some_and(|click| click.was_paused) {
                return;
            }
            if let Some(click) = pending
                && click.committed
            {
                // A slow double click: undo the toggle that already ran.
                if click.was_paused {
                    self.pause(cx);
                } else {
                    self.play(cx);
                }
            }
            let delta = match event.mouse_position() {
                Some(position) if position.x < self.bounds.get().center().x => -SKIP_SECONDS,
                _ => SKIP_SECONDS,
            };
            self.seek_by(delta, cx);
            self.feedback = Some((
                if delta > 0. {
                    Gesture::Forward
                } else {
                    Gesture::Backward
                },
                Instant::now(),
            ));
            cx.notify();
            return;
        }
        if held {
            return;
        }
        let was_paused = !self.playing;
        self.click = Some(PendingClick {
            at: Instant::now(),
            was_paused,
            committed: false,
        });
        self.feedback = Some((
            if was_paused {
                Gesture::Play
            } else {
                Gesture::Pause
            },
            Instant::now(),
        ));
        cx.notify();
    }

    fn inline(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        if self.state == State::Idle && self.playing {
            // A time chip asked for playback before the first play.
            self.load(cx);
        }
        let hovered = self.activity.is_some();
        let focused = window.last_input_was_keyboard()
            && (self.timeline_focus.is_focused(window) || self.volume_focus.is_focused(window));
        let open_menu = self.popup.is_some();
        let reveal = hovered || focused || open_menu;
        let reduce = cx.reduce_motion();
        let (center, center_moving) =
            self.center_fade
                .level(!self.playing || reveal, CONTROLS_FADE, reduce);
        let (controls, controls_moving) = self.controls_fade.level(
            reveal || (self.started && !self.playing),
            CONTROLS_FADE,
            reduce,
        );
        let (expand, expand_moving) = self.expand_fade.level(reveal, CONTROLS_FADE, reduce);
        if center_moving || controls_moving || expand_moving {
            window.request_animation_frame();
        }
        let width = f32::from(self.bounds.get().size.width);
        let narrow = width > 0. && width <= NARROW_PREVIEW;
        let poster = (!self.started)
            .then(|| self.clip.poster().or(self.captured.clone()))
            .flatten();
        let bounds = self.bounds.clone();
        div()
            .id("inline-video")
            .relative()
            .size_full()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if *hovered {
                    this.active(cx);
                } else {
                    this.left(cx);
                }
            }))
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, cx| this.active(cx)))
            .child(
                canvas(move |area, _, _| bounds.set(area), |_, _, _, _| {})
                    .absolute()
                    .inset_0()
                    .size_full(),
            )
            .when_some(
                self.frame.clone().filter(|_| self.started),
                |this, frame| {
                    this.child(
                        img(frame)
                            .absolute()
                            .inset_0()
                            .size_full()
                            .object_fit(ObjectFit::Cover),
                    )
                },
            )
            .when_some(poster, |this, poster| {
                this.child(
                    img(poster)
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Cover),
                )
            })
            .children(self.notice_view(&c))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div().opacity(center).child(
                            IconButton::new(
                                "play",
                                if self.playing {
                                    IconName::PauseFilled
                                } else {
                                    IconName::PlayFilled
                                },
                            )
                            .variant(ButtonVariant::Media)
                            .size(IconButtonSize::Md)
                            .tooltip(if self.playing {
                                "Pause video"
                            } else {
                                "Play video"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx))),
                        ),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left(px(6.))
                    .right(px(6.))
                    .bottom(px(6.))
                    .opacity(controls)
                    .when(controls == 0., |d| d.invisible())
                    .child(self.controls(true, narrow, window, cx)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(8.))
                    .right(px(8.))
                    .opacity(expand)
                    .when(expand == 0., |d| d.invisible())
                    .child(
                        IconButton::new("expand", IconName::ArrowsOut)
                            .variant(ButtonVariant::Media)
                            .size(IconButtonSize::Sm)
                            .tooltip("Open video fullscreen")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pause(cx);
                                if let Some(open) = this.open.clone() {
                                    let weak = cx.entity().downgrade();
                                    let seconds = this.position;
                                    window.defer(cx, move |window, cx| {
                                        open(seconds, weak, window, cx)
                                    });
                                }
                            })),
                    ),
            )
            .into_any_element()
    }

    fn notice_view(&self, c: &buzz_ui::Colors) -> Option<Div> {
        self.notice.clone().map(|notice| {
            div()
                .absolute()
                .top(px(12.))
                .left(px(12.))
                .right(px(12.))
                .p(px(8.))
                .rounded(px(8.))
                .bg(c.surface_inset)
                .text_caption()
                .text_color(c.text_standard)
                .child(notice)
        })
    }

    fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let focused = window.last_input_was_keyboard()
            && (self.timeline_focus.is_focused(window) || self.volume_focus.is_focused(window));
        let hide = self.playing
            && self.idle()
            && !focused
            && self.popup.is_none()
            && self.hold.as_ref().is_none_or(|hold| hold.held.is_none());
        let (opacity, moving) = self
            .controls_fade
            .level(!hide, CONTROLS_FADE, cx.reduce_motion());
        let feedback = self.feedback;
        if moving || feedback.is_some_and(|(kind, _)| kind != Gesture::Speed) {
            window.request_animation_frame();
        }
        let width = f32::from(self.bounds.get().size.width);
        let narrow = width > 0. && width < NARROW_PLAYER;
        let poster = self.frame.is_none().then(|| self.clip.poster()).flatten();
        let bounds = self.bounds.clone();
        let chrome = self.chrome.clone().map(|chrome| chrome(window, cx));
        div()
            .id("video-player")
            .relative()
            .size_full()
            .bg(c.bg_media_canvas)
            .overflow_hidden()
            .text_color(c.text_standard)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if *hovered {
                    this.active(cx);
                } else {
                    this.left(cx);
                }
            }))
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, cx| this.active(cx)))
            .on_scroll_wheel(cx.listener(|this, _: &ScrollWheelEvent, _, cx| this.active(cx)))
            .child(
                canvas(move |area, _, _| bounds.set(area), |_, _, _, _| {})
                    .absolute()
                    .inset_0()
                    .size_full(),
            )
            .child(
                div()
                    .id("video-picture")
                    .absolute()
                    .inset_0()
                    .when_some(self.frame.clone(), |this, frame| {
                        this.child(
                            img(frame)
                                .absolute()
                                .inset_0()
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        )
                    })
                    .when_some(poster, |this, poster| {
                        this.child(
                            img(poster)
                                .absolute()
                                .inset_0()
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.active(cx);
                            this.press(event.position, cx)
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_hold(cx)),
                    )
                    .on_click(
                        cx.listener(|this, event: &ClickEvent, _, cx| {
                            this.picture_click(event, cx)
                        }),
                    ),
            )
            .when(self.loading() && poster_missing(&self.clip), |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child("Loading media…"),
                )
            })
            .children(feedback.map(|(kind, at)| gesture_badge(kind, at, &c, cx.reduce_motion())))
            .children(self.notice_view(&c))
            .child(
                div()
                    .id("review-controls")
                    .absolute()
                    .left(px(16.))
                    .right(px(16.))
                    .bottom(px(12.))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(12.))
                    .gap_y(px(4.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(32.))
                    .bg(c.bg_media_glass.opacity(opacity))
                    .border_1()
                    .border_color(c.border_media_glass.opacity(opacity))
                    .when(opacity == 0., |d| d.invisible())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex_grow(1.)
                            .flex_shrink(1.)
                            .flex_basis(px(352.))
                            .min_w_0()
                            .opacity(opacity)
                            .child(self.controls(false, narrow, window, cx)),
                    )
                    .children(chrome.map(|chrome| div().opacity(opacity).mx_auto().child(chrome))),
            )
            .into_any_element()
    }
}

fn poster_missing(clip: &Clip) -> bool {
    clip.poster().is_none()
}

/// The transient gesture badge: a 40px media glass disc with the play or
/// pause glyph, a rotate arrow and "10" over the half that was double
/// clicked, or "2×" at the top while held. It grows from 0.88 to 1.08 and
/// fades over its last 30%.
fn gesture_badge(kind: Gesture, at: Instant, c: &buzz_ui::Colors, reduce: bool) -> AnyElement {
    let t = (at.elapsed().as_secs_f32() / FEEDBACK.as_secs_f32()).min(1.);
    let (scale, opacity) = if kind == Gesture::Speed || reduce {
        (1., 1.)
    } else {
        let eased = 1. - (1. - t) * (1. - t);
        let fade = if t <= 0.7 { 1. } else { 1. - (t - 0.7) / 0.3 };
        (0.88 + 0.2 * eased, fade)
    };
    let disc = 40. * scale;
    let content: AnyElement = match kind {
        Gesture::Play => Icon::new(IconName::PlayFilled)
            .size(24. * scale)
            .into_any_element(),
        Gesture::Pause => Icon::new(IconName::PauseFilled)
            .size(24. * scale)
            .into_any_element(),
        Gesture::Speed => div().text_label().child("2×").into_any_element(),
        Gesture::Forward | Gesture::Backward => div()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .child(
                Icon::new(if kind == Gesture::Forward {
                    IconName::ArrowClockwise
                } else {
                    IconName::ArrowCounterClockwise
                })
                .size(36. * scale),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12. * 0.8 * scale))
                    .font_features(tabular())
                    .child("10"),
            )
            .into_any_element(),
    };
    let badge = div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(disc))
        .rounded_full()
        .bg(c.bg_media_glass.opacity(opacity))
        .border_1()
        .border_color(c.border_media_glass.opacity(opacity))
        .text_color(c.text_on_media.opacity(opacity))
        .child(content);
    let lane = |fraction: f32| {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .child(div().w(relative(fraction)).flex_none())
            .child(div().w_0().flex().justify_center().child(badge))
    };
    match kind {
        Gesture::Speed => div()
            .absolute()
            .top(px(16.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .h(px(40.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .bg(c.bg_media_glass)
                    .border_1()
                    .border_color(c.border_media_glass)
                    .text_color(c.text_on_media)
                    .text_label()
                    .child("2×"),
            )
            .into_any_element(),
        Gesture::Forward => lane(0.75).into_any_element(),
        Gesture::Backward => lane(0.25).into_any_element(),
        Gesture::Play | Gesture::Pause => lane(0.5).into_any_element(),
    }
}

impl Render for VideoPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.window = Some(window.window_handle());
        self.wake(cx);
        match self.mode {
            Mode::Inline => self.inline(window, cx),
            Mode::Review => self.review(window, cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn trusts_only_believable_lengths() {
        assert_eq!(finite_duration(Some(12.5)), Some(12.5));
        assert_eq!(finite_duration(Some(0.)), None);
        assert_eq!(finite_duration(Some(f64::INFINITY)), None);
        assert_eq!(finite_duration(Some(90_000.)), None);
        assert_eq!(finite_duration(None), None);
    }

    #[test]
    fn fades_settle_at_their_ends() {
        let mut fade = Fade::new(false);
        assert_eq!(fade.level(false, CONTROLS_FADE, false), (0., false));
        let (level, moving) = fade.level(true, CONTROLS_FADE, false);
        assert!(moving && level < 0.5);
        assert_eq!(fade.level(true, CONTROLS_FADE, true), (1., false));
    }
}
