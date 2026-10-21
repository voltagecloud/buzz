//! The media review viewer (`MediaReviewViewer.tsx`, `ImageReviewStage.tsx`):
//! a dark dialog with the thread's image gallery or the video player,
//! beside the message's comments. The picture flies in from its thumbnail
//! and back on close (`use-review-entrance.ts`).
//!
//! One viewer is open at a time, app-wide, so any row can open it; the
//! workspace draws it above everything through [`review_overlay`]. The
//! same overlay holds a composer draft's preview dialog.

use super::super::composer::attachments::AttachmentDraft;
use super::comments::{self, Comments, ThreadState};
use super::draft_viewer::DraftViewer;
use super::video::{Chrome, Clip, IDLE, Marker, Mode, SKIP_SECONDS, VideoPlayer};
use super::{Remote, decoded, natural_size};
use crate::community::{AppState, Community};
use buzz_relay_client::fold::{Attachment, AttachmentKind, MessageRow};
use buzz_relay_client::{EventId, Media};
use buzz_ui::{
    ActiveTheme, Appearance, Button, ButtonSize, ButtonVariant, IconButton, IconButtonSize,
    IconName, PanelHeader, Spinner, ThemeScope, TypeStyled,
};
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIN_ZOOM: f32 = 0.5;
const BASE_MAX_ZOOM: f32 = 4.;
const ZOOM_STEP: f32 = 0.25;
const ZOOM_PRESETS: [f32; 4] = [0.5, 1., 1.5, 2.];
/// Pointer travel beyond this is a pan, not a click.
const CLICK_SLOP: f32 = 4.;
/// `.darkReviewBackdrop` padding, 12px below `50rem`, and the viewer's
/// largest size.
const BACKDROP_PADDING: f32 = 40.;
const NARROW_BACKDROP_PADDING: f32 = 12.;
const NARROW_WINDOW: f32 = 800.;
const MAX_SIZE: (f32, f32) = (1520., 980.);
/// At or below this viewer width the comments become a bottom sheet
/// (`@container media-review (max-width: 51.25rem)`).
const SHEET_WIDTH: f32 = 820.;
/// The comments sidebar's width (`minmax(0, 24rem)`).
const COMMENTS_WIDTH: f32 = 384.;
/// The shared-element flight (`SNAPPY`), and the chrome's fades.
pub(super) const FLIGHT: Duration = Duration::from_millis(240);
const CHROME_IN: Duration = Duration::from_millis(160);
const CHROME_DELAY: Duration = Duration::from_millis(80);
const CHROME_OUT: Duration = Duration::from_millis(100);
const FADE_OUT: Duration = Duration::from_millis(160);
/// The comments slide and the gallery slide.
const SLIDE: Duration = Duration::from_millis(200);
/// How far a gallery image slides as it changes.
const GALLERY_SHIFT: f32 = 40.;
/// Copy notices stay this long (`showCopyNotice`).
const COPIED: Duration = Duration::from_secs(4);
const COPY_FAILED: Duration = Duration::from_secs(6);
/// The reactions under a video (`VideoReviewReactions`).
const REACTIONS: [&str; 6] = ["😂", "😍", "😮", "🙌", "👍", "👎"];

/// `cubic-bezier(x1, y1, x2, y2)` at time `t`.
pub(super) fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    if t <= 0. || t >= 1. {
        return t.clamp(0., 1.);
    }
    let curve = |a: f32, b: f32, s: f32| {
        3. * a * s * (1. - s) * (1. - s) + 3. * b * s * s * (1. - s) + s * s * s
    };
    let (mut low, mut high) = (0., 1.);
    for _ in 0..24 {
        let mid = (low + high) / 2.;
        if curve(x1, x2, mid) < t {
            low = mid;
        } else {
            high = mid;
        }
    }
    curve(y1, y2, (low + high) / 2.)
}

/// The viewer's motion curve, `cubic-bezier(0.19, 1, 0.22, 1)`.
pub(super) fn snappy(t: f32) -> f32 {
    bezier(0.19, 1., 0.22, 1., t)
}

/// Development only: `BUZZ_DEV_SLOW_MOTION=<n>` plays the viewer's motion
/// `n` times slower, so screenshots can catch it mid-flight.
fn slow_motion() -> u32 {
    static FACTOR: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *FACTOR.get_or_init(|| {
        std::env::var("BUZZ_DEV_SLOW_MOTION")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|factor| *factor > 0)
            .unwrap_or(1)
    })
}

pub(super) fn progress(since: Instant, delay: Duration, duration: Duration) -> f32 {
    let elapsed = (since.elapsed() / slow_motion()).saturating_sub(delay);
    (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.)
}

pub(super) fn lerp_bounds(a: Bounds<Pixels>, b: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    let mix = |x: Pixels, y: Pixels| x + (y - x) * t;
    Bounds::new(
        point(mix(a.origin.x, b.origin.x), mix(a.origin.y, b.origin.y)),
        size(
            mix(a.size.width, b.size.width),
            mix(a.size.height, b.size.height),
        ),
    )
}

/// `aspect` fitted inside `bounds`, centred.
pub(super) fn contained(bounds: Bounds<Pixels>, aspect: f32) -> Bounds<Pixels> {
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let width = w.min(h * aspect);
    let height = width / aspect;
    Bounds::new(
        point(
            bounds.origin.x + px((w - width) / 2.),
            bounds.origin.y + px((h - height) / 2.),
        ),
        size(px(width), px(height)),
    )
}

/// Thumbnails remembered at most; the least recently painted go first.
const MAX_TILES: usize = 512;

type Tiles = HashMap<(WindowId, String), (Bounds<Pixels>, Instant)>;

thread_local! {
    /// Where each thumbnail was last painted, by window, so the picture
    /// can fly from and back to it.
    static TILES: RefCell<Tiles> = RefCell::default();
}

/// Records where a thumbnail of `url` is painted. Fills its `relative`
/// parent.
pub fn track_tile(url: &str) -> impl IntoElement + use<> {
    let url = url.to_string();
    canvas(
        move |bounds, window, _| {
            let key = (window.window_handle().window_id(), url);
            TILES.with_borrow_mut(|tiles| remember_tile(tiles, key, bounds, Instant::now()))
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
    .size_full()
}

fn remember_tile(tiles: &mut Tiles, key: (WindowId, String), bounds: Bounds<Pixels>, at: Instant) {
    tiles.insert(key, (bounds, at));
    if tiles.len() > MAX_TILES {
        // Keep the newer half.
        let mut times: Vec<Instant> = tiles.values().map(|(_, at)| *at).collect();
        times.sort_unstable();
        let cut = times[times.len() - MAX_TILES / 2];
        tiles.retain(|_, (_, at)| *at >= cut);
    }
}

fn tile_key(url: &str, window: &Window) -> (WindowId, String) {
    (window.window_handle().window_id(), url.to_string())
}

/// Where the thumbnail of `url` was last painted in `window`. An idle
/// window does not repaint, so this is where it still is when nothing
/// moved since.
pub(super) fn tile(url: &str, window: &Window) -> Option<Bounds<Pixels>> {
    let key = tile_key(url, window);
    TILES.with_borrow(|tiles| tiles.get(&key).map(|(bounds, _)| *bounds))
}

/// The thumbnail of `url` if it was painted since `since`: still on
/// screen while the viewer animates (each frame repaints the window).
pub(super) fn tile_since(url: &str, since: Instant, window: &Window) -> Option<Bounds<Pixels>> {
    let key = tile_key(url, window);
    TILES.with_borrow(|tiles| {
        tiles
            .get(&key)
            .filter(|(_, at)| *at >= since)
            .map(|(bounds, _)| *bounds)
    })
}

/// Whether the thumbnail of `url` hides because its picture is in flight.
pub fn tile_hidden(url: &str, cx: &App) -> bool {
    cx.try_global::<HostGlobal>()
        .and_then(|host| host.0.read(cx).review.clone())
        .is_some_and(|review| {
            let review = review.read(cx);
            review.origin.as_ref().is_some_and(|o| o.url == url) && review.motion.flying()
        })
}

struct Host {
    review: Option<Entity<Review>>,
    preview: Option<Entity<DraftViewer>>,
    /// Focus to give back on close.
    restore: Option<FocusHandle>,
}

struct HostGlobal(Entity<Host>);

impl Global for HostGlobal {}

fn host(cx: &mut App) -> Entity<Host> {
    if let Some(host) = cx.try_global::<HostGlobal>() {
        return host.0.clone();
    }
    let host = cx.new(|_| Host {
        review: None,
        preview: None,
        restore: None,
    });
    cx.set_global(HostGlobal(host.clone()));
    host
}

/// Opens `url` from `row`'s attachments: an image in the thread's
/// gallery, or a video in the player at `seconds`, beside the message's
/// comments. `inline` is the preview it came from, which follows the
/// viewer's time.
pub fn open_review(
    row: &Arc<MessageRow>,
    url: &str,
    media: &Media,
    seconds: f64,
    inline: Option<WeakEntity<VideoPlayer>>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(selected) = row.attachments.iter().find(|a| a.url == url).cloned() else {
        return;
    };
    let restore = window.focused(cx);
    let community = AppState::current(cx);
    let had_comments = row.reply_count > 0 || row.thread_root_id.is_some_and(|root| root != row.id);
    let origin = tile(url, window).map(|bounds| Origin {
        url: url.to_string(),
        bounds,
    });
    let row = row.clone();
    let review = cx.new(|cx| {
        Review::new(
            selected,
            media.clone(),
            seconds,
            inline,
            origin,
            community.map(|community| (community, row, had_comments)),
            window,
            cx,
        )
    });
    let focus = review.read(cx).focus.clone();
    focus.focus(window, cx);
    host(cx).update(cx, |host, cx| {
        host.review = Some(review);
        host.restore = restore;
        cx.notify();
    });
}

/// Opens the composer draft's file `selected` in the fullscreen draft
/// viewer (`ComposerAttachmentViewer`).
pub fn open_draft_preview(
    draft: Entity<AttachmentDraft>,
    selected: String,
    window: &mut Window,
    cx: &mut App,
) {
    let restore = window.focused(cx);
    let preview = cx.new(|cx| DraftViewer::new(draft, selected, window, cx));
    let focus = preview.read(cx).focus.clone();
    focus.focus(window, cx);
    host(cx).update(cx, |host, cx| {
        host.preview = Some(preview);
        host.restore = restore;
        cx.notify();
    });
}

/// Whether the draft tile of `id` hides because its picture is in flight.
pub fn draft_tile_hidden(id: &str, cx: &App) -> bool {
    cx.try_global::<HostGlobal>()
        .and_then(|host| host.0.read(cx).preview.clone())
        .is_some_and(|preview| preview.read(cx).flying(id))
}

/// Opens one attachment from outside the timeline (a GitHub body) in the
/// viewer, without a gallery or comments (`MediaViewer`).
pub fn open_media(attachment: Attachment, media: &Media, window: &mut Window, cx: &mut App) {
    open_media_at(attachment, media, 0., None, window, cx);
}

/// [`open_media`] for a video, from `seconds` and taking over the inline
/// player.
pub fn open_media_at(
    attachment: Attachment,
    media: &Media,
    seconds: f64,
    inline: Option<WeakEntity<VideoPlayer>>,
    window: &mut Window,
    cx: &mut App,
) {
    let restore = window.focused(cx);
    let media = media.clone();
    let review =
        cx.new(|cx| Review::new(attachment, media, seconds, inline, None, None, window, cx));
    let focus = review.read(cx).focus.clone();
    focus.focus(window, cx);
    host(cx).update(cx, |host, cx| {
        host.review = Some(review);
        host.restore = restore;
        cx.notify();
    });
}

/// The open viewer or draft preview, for the workspace to draw over the
/// window.
pub fn review_overlay(cx: &mut App) -> Option<impl IntoElement + use<>> {
    let host = host(cx).read(cx);
    let review = host.review.clone();
    let preview = host.preview.clone();
    if review.is_none() && preview.is_none() {
        return None;
    }
    Some(
        deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Window)
                .position(point(px(0.), px(0.)))
                .children(
                    review.map(|review| {
                        ThemeScope::new(Appearance::Dark, move |_, _| review.clone())
                    }),
                )
                .children(
                    preview.map(|preview| {
                        ThemeScope::new(Appearance::Dark, move |_, _| preview.clone())
                    }),
                ),
        )
        .with_priority(2),
    )
}

/// Closes the viewer now. Its last video frame leaves the sprite atlas,
/// which only the window can do.
fn close(window: &mut Window, cx: &mut App) {
    let (review, restore) = host(cx).update(cx, |host, cx| {
        cx.notify();
        (host.review.take(), host.restore.take())
    });
    if let Some(video) = review.and_then(|review| review.read(cx).video.clone()) {
        for image in video.update(cx, |video, _| video.take_frames()) {
            let _ = window.drop_image(image);
        }
    }
    if let Some(focus) = restore {
        focus.focus(window, cx);
    }
}

pub(super) fn close_preview(window: &mut Window, cx: &mut App) {
    let (preview, restore) = host(cx).update(cx, |host, cx| {
        cx.notify();
        (host.preview.take(), host.restore.take())
    });
    if let Some(preview) = preview {
        preview.update(cx, |preview, cx| preview.release(window, cx));
    }
    if let Some(focus) = restore {
        focus.focus(window, cx);
    }
}

/// The media review or a draft's preview is open: a dialog, which plugin
/// shortcuts without `allowInModal` wait out (`aria-modal`).
pub fn viewer_shown(cx: &App) -> bool {
    cx.try_global::<HostGlobal>().is_some_and(|host| {
        let host = host.0.read(cx);
        host.review.is_some() || host.preview.is_some()
    })
}

/// Pauses the viewer's video and closes the viewer, for a conversation
/// that is no longer shown (`useConversationPresentation`).
pub fn dismiss_review(window: &mut Window, cx: &mut App) {
    if host(cx).read(cx).review.is_some() {
        close(window, cx);
    }
}

/// The thumbnail the picture flies from and back to.
struct Origin {
    url: String,
    bounds: Bounds<Pixels>,
}

/// The shared-element entrance and exit.
enum Motion {
    /// The picture is not decoded yet; the chat thumbnail stays visible.
    Pending(Instant),
    Opening(Instant),
    Settled,
    Closing {
        started: Instant,
        /// The thumbnail it returns to, or `None` to fade out.
        to: Option<Bounds<Pixels>>,
        from: Bounds<Pixels>,
    },
}

impl Motion {
    fn flying(&self) -> bool {
        matches!(
            self,
            Self::Pending(_) | Self::Opening(_) | Self::Closing { to: Some(_), .. }
        )
    }
}

/// A press on the image stage, which ends as a click (zoom) or a pan.
struct Press {
    origin: Point<Pixels>,
    offset: Point<f32>,
    /// Only a press on the picture itself can zoom.
    click: bool,
}

/// The image leaving as the gallery changes.
struct Departing {
    url: String,
    direction: f32,
    /// Set once the arriving image is decoded and both move.
    started: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq)]
enum CopyNotice {
    Copied,
    Failed,
}

pub struct Review {
    focus: FocusHandle,
    media: Media,
    /// The attachment shown, and how many times one was chosen inside the
    /// viewer (`selectionRequest`).
    active: Attachment,
    request: u64,
    video: Option<Entity<VideoPlayer>>,
    inline: Option<WeakEntity<VideoPlayer>>,
    /// The image the gallery shows.
    selected: String,
    zoom: f32,
    /// Image offset from the stage centre, in pixels.
    offset: Point<f32>,
    press: Option<Press>,
    departing: Option<Departing>,
    copying: bool,
    copy_notice: Option<(CopyNotice, Instant)>,
    download_failed: Option<String>,
    /// Last pointer activity over the image stage; `None` once it left.
    activity: Option<Instant>,
    /// Redraws when the toolbar goes idle, for the activity it was set at.
    idle_timer: Option<(Instant, Task<()>)>,
    /// The stage's bounds as last drawn.
    stage: Rc<Cell<Bounds<Pixels>>>,
    community: Option<Entity<Community>>,
    comments: Option<Entity<Comments>>,
    had_comments: bool,
    /// The reader's choice; until then the sidebar follows the comments.
    choice: Option<bool>,
    sidebar: Option<Instant>,
    picker: Option<Entity<super::super::emoji_picker::EmojiPicker>>,
    picker_open: bool,
    reaction_error: Option<SharedString>,
    origin: Option<Origin>,
    motion: Motion,
    /// The picture's bounds as last drawn, where a close flies from.
    drawn: Option<Bounds<Pixels>>,
    _subscriptions: Vec<Subscription>,
}

impl Review {
    #[allow(clippy::too_many_arguments)]
    fn new(
        attachment: Attachment,
        media: Media,
        seconds: f64,
        inline: Option<WeakEntity<VideoPlayer>>,
        origin: Option<Origin>,
        thread: Option<(Entity<Community>, Arc<MessageRow>, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        let (community, comments, had_comments) = match thread {
            Some((community, row, had)) => {
                let (channel, id) = (row.channel.to_string(), row.id);
                let comments =
                    cx.new(|cx| Comments::new(community.clone(), channel, id, window, cx));
                let weak = cx.entity().downgrade();
                comments.update(cx, |comments, _| {
                    comments.set_on_open(Rc::new(move |_, attachment, seconds, _, cx| {
                        let attachment = attachment.clone();
                        let _ = weak.update(cx, |this, cx| this.select(attachment, seconds, cx));
                    }));
                });
                subscriptions.push(cx.observe(&comments, |this, _, cx| {
                    this.sync_markers(cx);
                    cx.notify();
                }));
                (Some(community), Some(comments), had)
            }
            None => (None, None, false),
        };
        let picker = community.as_ref().map(|community| {
            let picker = cx.new(|cx| {
                super::super::emoji_picker::EmojiPicker::new(community.clone(), window, cx)
                    .for_reactions()
            });
            subscriptions.push(cx.subscribe(
                &picker,
                |this, _, super::super::emoji_picker::Picked(pick), cx| {
                    // A closed picker sends nothing (`reaction/select`).
                    if !this.picker_open {
                        return;
                    }
                    this.picker_open = false;
                    this.react(&pick.text(), cx);
                },
            ));
            // A plugin picker's custom emoji, as it wrote it.
            subscriptions.push(cx.subscribe(
                &picker,
                |this, _, super::super::emoji_picker::PickedAs(text, _), cx| {
                    if !this.picker_open {
                        return;
                    }
                    this.picker_open = false;
                    this.react(text, cx);
                },
            ));
            subscriptions.push(cx.subscribe(
                &picker,
                |this, _, _: &super::super::emoji_picker::Dismissed, cx| {
                    this.picker_open = false;
                    cx.notify();
                },
            ));
            picker
        });
        let reduce = cx.reduce_motion();
        let mut review = Self {
            focus: cx.focus_handle().tab_stop(true),
            selected: attachment.url.clone(),
            active: attachment,
            media,
            request: 0,
            video: None,
            inline,
            zoom: 1.,
            offset: point(0., 0.),
            press: None,
            departing: None,
            copying: false,
            copy_notice: None,
            download_failed: None,
            activity: Some(Instant::now()),
            stage: Rc::default(),
            community,
            comments,
            had_comments,
            choice: None,
            sidebar: None,
            picker,
            picker_open: false,
            reaction_error: None,
            motion: if origin.is_some() && !reduce {
                Motion::Pending(Instant::now())
            } else {
                Motion::Settled
            },
            origin: origin.filter(|_| !reduce),
            drawn: None,
            idle_timer: None,
            _subscriptions: subscriptions,
        };
        review.show(seconds, cx);
        review
    }

    /// Builds the stage for the active attachment.
    fn show(&mut self, seconds: f64, cx: &mut Context<Self>) {
        self.selected = self.active.url.clone();
        self.zoom = 1.;
        self.offset = point(0., 0.);
        self.press = None;
        self.departing = None;
        self.copy_notice = None;
        self.video = (self.active.kind == AttachmentKind::Video).then(|| {
            let clip = Clip::Remote {
                attachment: self.active.clone(),
                media: self.media.clone(),
            };
            let video = cx.new(|cx| VideoPlayer::new(clip, Mode::Review, seconds, cx));
            let inline = self.inline.clone().filter(|_| self.request == 0);
            self._subscriptions
                .push(cx.observe(&video, move |_, video, cx| {
                    // The preview follows the viewer's time (`onTime`).
                    if let Some(inline) = inline.as_ref().and_then(WeakEntity::upgrade) {
                        let seconds = video.read(cx).position();
                        inline.update(cx, |inline, cx| inline.follow(seconds, cx));
                    }
                    cx.notify();
                }));
            video
        });
        if let Some(comments) = &self.comments {
            let video = self.video.as_ref().map(Entity::downgrade);
            comments.update(cx, |comments, cx| comments.set_video(video, cx));
        }
        self.sync_markers(cx);
        let chrome = self.reactions_chrome(cx);
        if let Some(video) = &self.video {
            video.update(cx, |video, _| video.set_chrome(chrome));
        }
        cx.notify();
    }

    /// An attachment chosen inside the viewer: a comment's media.
    fn select(&mut self, attachment: Attachment, seconds: f64, cx: &mut Context<Self>) {
        self.request += 1;
        if attachment.kind == AttachmentKind::Image && self.video.is_none() {
            self.active = attachment;
            let url = self.active.url.clone();
            self.choose(url, cx);
            return;
        }
        if attachment.url == self.active.url
            && let Some(video) = &self.video
        {
            video.update(cx, |video, cx| video.seek_to(seconds, cx));
            return;
        }
        self.active = attachment;
        self.show(seconds, cx);
    }

    /// Timecoded comments as timeline markers (`markers`).
    fn sync_markers(&mut self, cx: &mut Context<Self>) {
        let (Some(video), Some(comments), Some(community)) =
            (&self.video, &self.comments, &self.community)
        else {
            return;
        };
        let comments_read = comments.read(cx);
        if comments_read.state() != ThreadState::Ready {
            return;
        }
        let replies = comments_read.replies();
        let community = community.read(cx);
        let profiles = &community.snapshot.profiles;
        let media = community.session.media().clone();
        let markers: Vec<Marker> = replies
            .iter()
            .filter_map(|row| {
                let reply = super::timecode::parse(&row.content)?;
                let display = row.workflow_owner.unwrap_or(row.author).to_hex();
                let profile = profiles.get(&display);
                let name = profile.map_or_else(
                    || display.chars().take(10).collect::<String>(),
                    |p| p.name.clone(),
                );
                Some(Marker {
                    id: row.id,
                    seconds: reply.seconds,
                    label: reply.label,
                    author: if row.workflow_owner.is_some() {
                        format!("{name} workflow").into()
                    } else {
                        name.into()
                    },
                    picture: profile
                        .and_then(|p| p.picture.as_deref())
                        .filter(|_| row.workflow_owner.is_none())
                        .map(|picture| super::avatar(picture, &media)),
                    text: reply.content,
                })
            })
            .collect();
        let weak = cx.entity().downgrade();
        video.update(cx, |video, _| {
            video.set_markers(
                markers,
                Rc::new(move |id: EventId, _: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        this.choice = Some(true);
                        if let Some(comments) = &this.comments {
                            comments.update(cx, |comments, cx| comments.select(id, cx));
                        }
                        cx.notify();
                    });
                }),
            )
        });
    }

    /// The quick reactions under the video, once the thread resolved.
    fn reactions_chrome(&self, cx: &mut Context<Self>) -> Option<Chrome> {
        self.comments.as_ref()?;
        let weak = cx.entity().downgrade();
        Some(Rc::new(move |window: &mut Window, cx: &mut App| {
            weak.update(cx, |this, cx| this.reactions(window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        }))
    }

    /// Whether reactions can post: the channel is listed and writable.
    fn can_react(&self, cx: &App) -> bool {
        let (Some(community), Some(comments)) = (&self.community, &self.comments) else {
            return false;
        };
        let channel = comments.read(cx).channel().to_string();
        community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .any(|summary| summary.id == channel && !summary.read_only)
    }

    /// Posts `⏱ m:ss — <emoji>` to the thread, pausing the video there.
    fn react(&mut self, emoji: &str, cx: &mut Context<Self>) {
        if !self.can_react(cx) {
            return;
        }
        let (Some(community), Some(comments), Some(video)) =
            (&self.community, &self.comments, &self.video)
        else {
            return;
        };
        video.update(cx, |video, cx| video.pause(cx));
        let seconds = video.read(cx).position();
        let comments = comments.read(cx);
        let Some(root) = comments.root_id() else {
            return;
        };
        let channel = comments.channel().to_string();
        let session = community.read(cx).session.clone();
        let content = super::timecode::time_reply(seconds, emoji);
        self.reaction_error = None;
        cx.spawn(async move |this, cx| {
            let result = session
                .send(
                    &channel,
                    &content,
                    Default::default(),
                    Default::default(),
                    Some((root, root)),
                )
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |this, cx| {
                    let error = error.to_string();
                    this.reaction_error = Some(if error.is_empty() {
                        "Could not post reaction.".into()
                    } else {
                        error.into()
                    });
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn reactions(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let resolved = self
            .comments
            .as_ref()
            .is_some_and(|comments| comments.read(cx).state() == ThreadState::Ready);
        if !resolved {
            return div().into_any_element();
        }
        let disabled = !self.can_react(cx);
        let picker = self.picker.clone().filter(|_| self.picker_open);
        div()
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .p(px(4.))
                    .children(REACTIONS.iter().map(|emoji| {
                        let emoji = *emoji;
                        IconButton::new(
                            SharedString::from(format!("react-{emoji}")),
                            IconName::Smiley,
                        )
                        .size(IconButtonSize::Sm)
                        .disabled(disabled)
                        .tooltip(format!("React {emoji} at current frame"))
                        .child(
                            div()
                                .text_size(px(22. * 0.85))
                                .line_height(px(22.))
                                .when(emoji == "🙌", |d| d.mt(px(-2.)))
                                .child(emoji),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.react(emoji, cx)))
                    }))
                    .when_some(self.picker.clone(), |this, _| {
                        this.child(
                            div()
                                .relative()
                                .child(
                                    IconButton::new("react-more", IconName::SmileyPlus)
                                        .size(IconButtonSize::Sm)
                                        .disabled(disabled)
                                        .selected(self.picker_open)
                                        .tooltip("Add reaction")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.picker_open = !this.picker_open;
                                            if let Some(picker) = &this.picker {
                                                picker.update(cx, |picker, cx| {
                                                    picker.reset(window, cx);
                                                    picker.focus(window, cx);
                                                });
                                            }
                                            cx.notify();
                                        })),
                                )
                                .when_some(picker, |this, picker| {
                                    let weak = cx.entity().downgrade();
                                    this.child(
                                        div().absolute().top_0().left(relative(0.5)).child(
                                            deferred(
                                                anchored()
                                                    .anchor(Anchor::BottomCenter)
                                                    .offset(point(px(0.), px(-8.)))
                                                    .snap_to_window_with_margin(px(8.))
                                                    .child(
                                                        super::super::emoji_picker::emoji_picker(
                                                            &picker,
                                                            move |_, _, cx| {
                                                                let _ =
                                                                    weak.update(cx, |this, cx| {
                                                                        this.picker_open = false;
                                                                        cx.notify();
                                                                    });
                                                            },
                                                        ),
                                                    ),
                                            )
                                            .with_priority(3),
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .children(self.reaction_error.clone().map(|error| {
                div()
                    .p(px(8.))
                    .bg(c.affordance_navigation_quiet)
                    .text_caption()
                    .text_color(c.text_standard)
                    .child(error)
            }))
            .into_any_element()
    }

    fn comments_open(&self, cx: &App) -> bool {
        if let Some(choice) = self.choice {
            return choice;
        }
        match &self.comments {
            Some(comments) => {
                let comments = comments.read(cx);
                match comments.state() {
                    ThreadState::Loading => self.had_comments,
                    ThreadState::Failed(_) | ThreadState::Missing => true,
                    ThreadState::Ready => comments.has_comments(),
                }
            }
            None => false,
        }
    }

    fn toggle_comments(&mut self, pointer: bool, cx: &mut Context<Self>) {
        let open = !self.comments_open(cx);
        self.choice = Some(open);
        self.sidebar = (pointer && !cx.reduce_motion()).then(Instant::now);
        cx.notify();
    }

    /// The sidebar's share of its width, 0..=1, while it slides.
    fn sidebar_level(&self, open: bool) -> (f32, bool) {
        let target = if open { 1. } else { 0. };
        match self.sidebar {
            Some(started) => {
                let t = snappy(progress(started, Duration::ZERO, SLIDE));
                let level = if open { t } else { 1. - t };
                (level, (level - target).abs() > f32::EPSILON)
            }
            None => (target, false),
        }
    }

    /// Closes with the motion: back to the thumbnail when it is on screen
    /// and the picture is the one opened, unzoomed; otherwise a fade.
    fn dismiss(&mut self, pointer: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(video) = &self.video {
            video.update(cx, |video, cx| video.pause(cx));
        }
        if matches!(self.motion, Motion::Closing { .. }) {
            return;
        }
        if !pointer || cx.reduce_motion() || matches!(self.motion, Motion::Pending(_)) {
            window.defer(cx, close);
            return;
        }
        let from = self.drawn;
        let viewport = window.viewport_size();
        let to = self
            .origin
            .as_ref()
            .filter(|origin| self.request == 0 && self.selected == origin.url && self.zoom == 1.)
            .and_then(|origin| tile(&origin.url, window))
            .filter(|bounds| {
                bounds.size.width > px(0.)
                    && bounds.right() > px(0.)
                    && bounds.bottom() > px(0.)
                    && bounds.left() < viewport.width
                    && bounds.top() < viewport.height
            });
        let Some(from) = from else {
            window.defer(cx, close);
            return;
        };
        self.motion = Motion::Closing {
            started: Instant::now(),
            to,
            from,
        };
        cx.notify();
    }

    /// The picture's bounds in the window, when it has a size.
    fn picture_bounds(&self, window: &mut Window, cx: &mut App) -> Option<Bounds<Pixels>> {
        let stage = self.stage.get();
        if stage.size.width <= px(0.) {
            return None;
        }
        let natural = match &self.video {
            Some(video) => video.read(cx).natural(),
            None => self.natural(window, cx),
        }?;
        let aspect = natural.width / natural.height;
        if !aspect.is_finite() || aspect <= 0. {
            return None;
        }
        let base = contained(stage, aspect);
        if self.video.is_some() || self.zoom == 1. {
            return Some(base);
        }
        let scaled = size(base.size.width * self.zoom, base.size.height * self.zoom);
        Some(Bounds::new(
            point(
                stage.center().x - scaled.width / 2. + px(self.offset.x),
                stage.center().y - scaled.height / 2. + px(self.offset.y),
            ),
            scaled,
        ))
    }

    /// The flying picture's pixels.
    fn picture(&self, window: &mut Window, cx: &mut App) -> Option<ImageSource> {
        match &self.video {
            Some(video) => {
                let video = video.read(cx);
                video.frame().map(ImageSource::from).or_else(|| {
                    self.active
                        .preview_url
                        .as_ref()
                        .map(|url| super::image(url, &self.media))
                })
            }
            None => {
                let remote = Remote {
                    url: self.selected.clone().into(),
                    fallback: None,
                    media: self.media.clone(),
                };
                match decoded(&remote, window, cx)? {
                    Ok(image) => Some(ImageSource::from(image)),
                    Err(_) => None,
                }
            }
        }
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if !matches!(self.motion, Motion::Settled | Motion::Closing { .. }) {
            self.motion = Motion::Settled;
        }
        let typing = self
            .comments
            .as_ref()
            .is_some_and(|comments| comments.read(cx).typing(window, cx));
        if typing && keystroke.key != "escape" {
            return;
        }
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.shift {
            return;
        }
        let gallery = self.gallery(cx);
        let index = gallery.iter().position(|a| a.url == self.selected);
        match (keystroke.key.as_str(), &self.video) {
            ("escape", _) => {
                if let Some(video) = &self.video {
                    video.update(cx, |video, cx| video.pause(cx));
                }
                // Closing reads this view, which is busy handling the key.
                window.defer(cx, close);
            }
            ("left", Some(video)) => video.update(cx, |v, cx| v.seek_by(-SKIP_SECONDS, cx)),
            ("right", Some(video)) => video.update(cx, |v, cx| v.seek_by(SKIP_SECONDS, cx)),
            ("space", Some(video)) if !event.is_held => video.update(cx, |v, cx| v.toggle(cx)),
            ("left", None) if gallery.len() > 1 && index.is_some_and(|i| i > 0) => {
                let url = gallery[index.unwrap_or(0) - 1].url.clone();
                self.choose(url, cx);
            }
            ("right", None)
                if gallery.len() > 1 && index.is_some_and(|i| i + 1 < gallery.len()) =>
            {
                let url = gallery[index.unwrap_or(0) + 1].url.clone();
                self.choose(url, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The thread's images, else the opened one alone.
    fn gallery(&self, cx: &App) -> Vec<Attachment> {
        let rows = self
            .comments
            .as_ref()
            .map(|comments| comments.read(cx).rows())
            .unwrap_or_default();
        comments::gallery(&rows, &self.active)
    }

    fn choose(&mut self, url: String, cx: &mut Context<Self>) {
        if url == self.selected {
            return;
        }
        let gallery = self.gallery(cx);
        let from = gallery.iter().position(|a| a.url == self.selected);
        let to = gallery.iter().position(|a| a.url == url);
        let direction = match (from, to) {
            (Some(from), Some(to)) if to < from => -1.,
            _ => 1.,
        };
        if !cx.reduce_motion() {
            // Rapid changes keep the last image that was drawn.
            let keep = self
                .departing
                .as_ref()
                .filter(|d| d.started.is_none())
                .map(|d| d.url.clone());
            self.departing = Some(Departing {
                url: keep.unwrap_or_else(|| self.selected.clone()),
                direction,
                started: None,
            });
        }
        self.selected = url;
        self.zoom = 1.;
        self.offset = point(0., 0.);
        self.press = None;
        self.copy_notice = None;
        self.download_failed = None;
        cx.notify();
    }

    /// Display scale of the fitted image; 1 is its own pixels.
    fn fit(&self, natural: Size<f32>) -> Option<f32> {
        let stage = self.stage.get().size;
        let (width, height) = (f32::from(stage.width), f32::from(stage.height));
        (natural.width > 0. && natural.height > 0. && width > 0. && height > 0.)
            .then(|| (width / natural.width).min(height / natural.height))
    }

    fn max_zoom(&self, natural: Option<Size<f32>>) -> f32 {
        natural
            .and_then(|n| self.fit(n))
            .map_or(BASE_MAX_ZOOM, |fit| BASE_MAX_ZOOM.max(2. / fit))
    }

    /// Keeps the image covering the stage wherever it is larger than it.
    fn clamp_offset(
        &self,
        zoom: f32,
        offset: Point<f32>,
        natural: Option<Size<f32>>,
    ) -> Point<f32> {
        let Some((natural, fit)) = natural.and_then(|n| Some((n, self.fit(n)?))) else {
            return point(0., 0.);
        };
        let stage = self.stage.get().size;
        let limit_x = ((natural.width * fit * zoom - f32::from(stage.width)) / 2.).max(0.);
        let limit_y = ((natural.height * fit * zoom - f32::from(stage.height)) / 2.).max(0.);
        point(
            offset.x.clamp(-limit_x, limit_x),
            offset.y.clamp(-limit_y, limit_y),
        )
    }

    /// `zoomTo`: zooms about `anchor` (from the stage centre), or about the
    /// current offset.
    fn zoom_to(
        &mut self,
        value: f32,
        anchor: Option<Point<f32>>,
        natural: Option<Size<f32>>,
        cx: &mut Context<Self>,
    ) {
        if !value.is_finite() {
            return;
        }
        let zoom = value.clamp(MIN_ZOOM, self.max_zoom(natural));
        let ratio = zoom / self.zoom;
        let offset = match anchor {
            Some(anchor) => point(
                anchor.x - (anchor.x - self.offset.x) * ratio,
                anchor.y - (anchor.y - self.offset.y) * ratio,
            ),
            None => self.offset,
        };
        self.zoom = zoom;
        self.offset = self.clamp_offset(zoom, offset, natural);
        cx.notify();
    }

    /// From a window position to an offset from the stage centre.
    fn stage_point(&self, position: Point<Pixels>) -> Point<f32> {
        let center = self.stage.get().center();
        point(
            f32::from(position.x - center.x),
            f32::from(position.y - center.y),
        )
    }

    fn natural(&self, window: &mut Window, cx: &mut App) -> Option<Size<f32>> {
        self.natural_of(&self.selected, window, cx)
    }

    fn natural_of(&self, url: &str, window: &mut Window, cx: &mut App) -> Option<Size<f32>> {
        natural_size(url, &self.media, window, cx).or_else(|| {
            self.gallery(cx)
                .iter()
                .find(|a| a.url == url)
                .and_then(|a| a.dimensions)
                .map(|d| size(d.width as f32, d.height as f32))
        })
    }

    /// The picture's rect inside the stage at `zoom` and `offset`.
    fn image_rect(
        &self,
        natural: Size<f32>,
        zoom: f32,
        offset: Point<f32>,
    ) -> Option<Bounds<Pixels>> {
        let fit = self.fit(natural)?;
        let stage = self.stage.get();
        let (width, height) = (natural.width * fit * zoom, natural.height * fit * zoom);
        Some(Bounds::new(
            point(
                px((f32::from(stage.size.width) - width) / 2. + offset.x),
                px((f32::from(stage.size.height) - height) / 2. + offset.y),
            ),
            size(px(width), px(height)),
        ))
    }

    fn copy_image(&mut self, cx: &mut Context<Self>) {
        if self.copying {
            return;
        }
        self.copying = true;
        self.copy_notice = None;
        let (url, media) = (self.selected.clone(), self.media.clone());
        cx.spawn(async move |this, cx| {
            // buzz-desktop fetches and decodes the authenticated image; the
            // app-lifetime clipboard owner (`image_clipboard`) keeps it
            // available on Linux, where GPUI's clipboard is text-only on
            // Wayland.
            let pixels = media.copy_image(&url).await;
            let copied = match pixels {
                Ok((width, height, rgba)) => {
                    cx.background_executor()
                        .spawn(async move { crate::image_clipboard::write(width, height, rgba) })
                        .await
                }
                Err(error) => Err(error),
            };
            let _ = this.update(cx, |this, cx| {
                this.copying = false;
                let notice = match copied {
                    Ok(()) => CopyNotice::Copied,
                    Err(error) => {
                        tracing::warn!(%error, "could not copy the image");
                        CopyNotice::Failed
                    }
                };
                // A notice belongs to the image it was copied from.
                if this.selected == url {
                    this.copy_notice = Some((notice, Instant::now()));
                    let wait = if notice == CopyNotice::Copied {
                        COPIED
                    } else {
                        COPY_FAILED
                    };
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        let _ = this.update(cx, |this, cx| {
                            if this.copy_notice.is_some_and(|(_, at)| at.elapsed() >= wait) {
                                this.copy_notice = None;
                                cx.notify();
                            }
                        });
                    })
                    .detach();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn download(&mut self, cx: &mut Context<Self>) {
        let (url, media) = (self.selected.clone(), self.media.clone());
        let name = self
            .gallery(cx)
            .into_iter()
            .find(|a| a.url == url)
            .and_then(|a| a.name);
        self.download_failed = None;
        cx.spawn(async move |this, cx| {
            if super::save(&media, &url, name.as_deref()).await.is_err() {
                let _ = this.update(cx, |this, cx| {
                    this.download_failed = Some(url);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn image_stage(
        &mut self,
        hidden: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let gallery = self.gallery(cx);
        let count = gallery.len();
        let index = gallery
            .iter()
            .position(|a| a.url == self.selected)
            .unwrap_or(0);
        let remote = |url: &str| Remote {
            url: url.to_string().into(),
            fallback: None,
            media: self.media.clone(),
        };
        let image = decoded(&remote(&self.selected), window, cx);
        let natural = self.natural(window, cx);
        // Re-clamp the pan when the stage changed size.
        let clamped = self.clamp_offset(self.zoom, self.offset, natural);
        if clamped != self.offset {
            self.offset = clamped;
        }
        let fit = natural.and_then(|n| self.fit(n));
        let max_zoom = self.max_zoom(natural);
        let pannable = self.zoom > 1.;
        let next_zoom = ZOOM_PRESETS
            .into_iter()
            .find(|preset| *preset > self.zoom)
            .unwrap_or(MIN_ZOOM);
        let picture_rect = natural.and_then(|n| self.image_rect(n, self.zoom, self.offset));
        // The arriving image waits for its pixels; then both slide.
        let reduce = cx.reduce_motion();
        let mut slide: Option<(f32, f32)> = None;
        let mut departing_element = None;
        if let Some(departing) = self.departing.as_mut() {
            if departing.started.is_none() && matches!(image, Some(Ok(_)) | Some(Err(_))) {
                departing.started = Some(Instant::now());
            }
            let t = departing.started.map_or(0., |started| {
                snappy(progress(started, Duration::ZERO, SLIDE))
            });
            if t >= 1. || reduce {
                self.departing = None;
            } else {
                window.request_animation_frame();
                let direction = departing.direction;
                slide = departing
                    .started
                    .map(|_| (direction * GALLERY_SHIFT * (1. - t), t));
                let url = departing.url.clone();
                let old = decoded(&remote(&url), window, cx).and_then(Result::ok);
                let old_natural = natural_size(&url, &self.media, window, cx);
                if let (Some(old), Some(old_rect)) = (
                    old,
                    old_natural.and_then(|n| self.image_rect(n, 1., point(0., 0.))),
                ) {
                    departing_element = Some(
                        img(old)
                            .absolute()
                            .left(old_rect.origin.x - px(direction * GALLERY_SHIFT * t))
                            .top(old_rect.origin.y)
                            .w(old_rect.size.width)
                            .h(old_rect.size.height)
                            .opacity(1. - t),
                    );
                }
            }
        }
        let loading_next = self.departing.as_ref().is_some_and(|d| d.started.is_none());
        let picture = match (&image, picture_rect) {
            (Some(Err(_)), _) => unavailable("Image unavailable", &c).into_any_element(),
            (Some(Ok(image)), Some(rect)) => {
                let (shift, opacity) = slide.unwrap_or((0., 1.));
                div()
                    .id("review-picture")
                    .absolute()
                    .left(rect.origin.x + px(shift))
                    .top(rect.origin.y)
                    .w(rect.size.width)
                    .h(rect.size.height)
                    .when(!pannable, |d| d.cursor_pointer())
                    .when(hidden || loading_next, |d| d.invisible())
                    .child(img(image.clone()).size_full().opacity(opacity))
                    .into_any_element()
            }
            _ => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_body_sm()
                .text_color(c.text_subtle)
                .when(self.departing.is_some(), |d| d.invisible())
                .child("Loading media…")
                .into_any_element(),
        };
        let relay = self.media.is_relay_media(&self.selected);
        let stage_bounds = self.stage.clone();
        let browser_url = self.selected.clone();
        let copy_visible = self.copying || self.copy_notice.is_some();
        let focused = window.last_input_was_keyboard() && self.focus.contains_focused(window, cx);
        let idle = self.activity.is_none_or(|at| at.elapsed() >= IDLE);
        let toolbar_hidden = idle && !copy_visible && !focused;
        // One timer per activity, not one per draw.
        if let Some(at) = self.activity
            && at.elapsed() < IDLE
            && self.idle_timer.as_ref().is_none_or(|(set, _)| *set != at)
        {
            let wait = IDLE - at.elapsed();
            let task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            });
            self.idle_timer = Some((at, task));
        }
        let download_failed = self.download_failed.as_deref() == Some(self.selected.as_str());
        div()
            .id("image-stage")
            .relative()
            .size_full()
            .overflow_hidden()
            .when(pannable, |d| {
                d.cursor(if self.press.is_some() {
                    CursorStyle::ClosedHand
                } else {
                    CursorStyle::OpenHand
                })
            })
            .child(
                canvas(
                    move |bounds, window, _| {
                        if stage_bounds.replace(bounds) != bounds {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0()
                .size_full(),
            )
            .children(departing_element)
            .child(picture)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.activity = hovered.then(Instant::now);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.activity = Some(Instant::now());
                    if let Some(press) = this.press.as_mut() {
                        // A second contact is not a click.
                        press.click = false;
                        return;
                    }
                    let stage = this.stage.get();
                    let on_picture = picture_rect.is_some_and(|rect| {
                        Bounds::new(rect.origin + stage.origin, rect.size).contains(&event.position)
                    });
                    this.press = Some(Press {
                        origin: event.position,
                        offset: this.offset,
                        click: on_picture,
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let idle = this.activity.is_none_or(|at| at.elapsed() >= IDLE);
                this.activity = Some(Instant::now());
                if idle {
                    cx.notify();
                }
                let Some(press) = this.press.as_mut() else {
                    return;
                };
                if !event.dragging() {
                    this.press = None;
                    cx.notify();
                    return;
                }
                let delta = event.position - press.origin;
                let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
                if dx.hypot(dy) > CLICK_SLOP {
                    press.click = false;
                }
                if this.zoom > 1. {
                    let offset = point(press.offset.x + dx, press.offset.y + dy);
                    this.offset = this.clamp_offset(this.zoom, offset, natural);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    let Some(press) = this.press.take() else {
                        return;
                    };
                    cx.notify();
                    if !press.click {
                        return;
                    }
                    // Click zoom: fit goes to actual pixels for downscaled
                    // images, and at least 2x otherwise; any other zoom
                    // returns to fit.
                    if this.zoom != 1. {
                        this.zoom_to(1., None, natural, cx);
                    } else {
                        let target = fit.map_or(2., |fit| (1. / fit).max(2.));
                        let anchor = this.stage_point(event.position);
                        this.zoom_to(target, Some(anchor), natural, cx);
                    }
                }),
            )
            .on_scroll_wheel(
                cx.listener(move |this, event: &ScrollWheelEvent, window, cx| {
                    this.activity = Some(Instant::now());
                    let delta = event.delta.pixel_delta(window.line_height());
                    let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
                    if event.modifiers.control {
                        let anchor = this.stage_point(event.position);
                        this.zoom_to(this.zoom * (dy * 0.01).exp(), Some(anchor), natural, cx);
                    } else if this.zoom > 1. {
                        let offset = point(this.offset.x + dx, this.offset.y + dy);
                        this.offset = this.clamp_offset(this.zoom, offset, natural);
                        cx.notify();
                    }
                    cx.stop_propagation();
                }),
            )
            .on_pinch(cx.listener(move |this, event: &PinchEvent, _, cx| {
                let anchor = this.stage_point(event.position);
                this.zoom_to(this.zoom * (1. + event.delta), Some(anchor), natural, cx);
            }))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(16.))
                    .flex()
                    .justify_center()
                    .when(toolbar_hidden || hidden, |d| d.invisible())
                    .child(
                        div()
                            .id("image-controls")
                            .max_w(self.stage.get().size.width - px(32.))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_center()
                            .gap_x(px(12.))
                            .gap_y(px(4.))
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(32.))
                            .bg(c.bg_media_glass)
                            .border_1()
                            .border_color(c.border_media_glass)
                            .text_color(c.text_on_media)
                            .text_body_sm()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .when(count > 1, |this| {
                                let previous =
                                    gallery.get(index.wrapping_sub(1)).map(|a| a.url.clone());
                                let next = gallery.get(index + 1).map(|a| a.url.clone());
                                this.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(4.))
                                        .child(
                                            IconButton::new("previous", IconName::CaretLeft)
                                                .size(IconButtonSize::Sm)
                                                .icon_size(18.)
                                                .tooltip("Previous image")
                                                .disabled(previous.is_none())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if let Some(url) = previous.clone() {
                                                        this.choose(url, cx);
                                                    }
                                                })),
                                        )
                                        .child(
                                            div()
                                                .min_w(px(45.))
                                                .flex()
                                                .justify_center()
                                                .text_caption()
                                                .font_features(super::tabular())
                                                .child(format!("{} / {count}", index + 1)),
                                        )
                                        .child(
                                            IconButton::new("next", IconName::CaretRight)
                                                .size(IconButtonSize::Sm)
                                                .icon_size(18.)
                                                .tooltip("Next image")
                                                .disabled(next.is_none())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if let Some(url) = next.clone() {
                                                        this.choose(url, cx);
                                                    }
                                                })),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(
                                        IconButton::new("zoom-out", IconName::Minus)
                                            .size(IconButtonSize::Sm)
                                            .tooltip("Zoom out")
                                            .disabled(self.zoom <= MIN_ZOOM)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.zoom_to(
                                                    this.zoom - ZOOM_STEP,
                                                    None,
                                                    natural,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        Button::new(
                                            "zoom",
                                            format!("{}%", (self.zoom * 100.).round()),
                                        )
                                        .variant(ButtonVariant::Ghost)
                                        .size(ButtonSize::Sm)
                                        .tooltip(format!(
                                            "Image zoom: {}%. Zoom to {}%",
                                            (self.zoom * 100.).round(),
                                            next_zoom * 100.
                                        ))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.zoom_to(next_zoom, None, natural, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        IconButton::new("zoom-in", IconName::Plus)
                                            .size(IconButtonSize::Sm)
                                            .tooltip("Zoom in")
                                            .disabled(self.zoom >= max_zoom)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.zoom_to(
                                                    this.zoom + ZOOM_STEP,
                                                    None,
                                                    natural,
                                                    cx,
                                                )
                                            })),
                                    ),
                            )
                            .when(relay, |this| {
                                this.child(if self.copying {
                                    div()
                                        .size(px(32.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(Spinner::new().size(17.))
                                        .into_any_element()
                                } else {
                                    IconButton::new("copy", IconName::Copy)
                                        .size(IconButtonSize::Sm)
                                        .icon_size(17.)
                                        .tooltip("Copy image")
                                        .on_click(cx.listener(|this, _, _, cx| this.copy_image(cx)))
                                        .into_any_element()
                                })
                            })
                            .children(self.copy_notice.map(|(notice, _)| {
                                div().child(match notice {
                                    CopyNotice::Copied => "Image copied",
                                    CopyNotice::Failed => "Couldn't copy image",
                                })
                            }))
                            .when(relay, |this| {
                                this.child(
                                    IconButton::new("download", IconName::Download)
                                        .size(IconButtonSize::Sm)
                                        .icon_size(17.)
                                        .tooltip("Download image")
                                        .on_click(cx.listener(|this, _, _, cx| this.download(cx))),
                                )
                            })
                            .when(download_failed, |this| {
                                this.child(div().child("Download failed. Try again."))
                            })
                            .when(!relay, |this| {
                                this.child(
                                    IconButton::new("browser", IconName::ArrowSquareOut)
                                        .size(IconButtonSize::Sm)
                                        .icon_size(17.)
                                        .tooltip("Open image in browser")
                                        // As a link opens: in a panel that
                                        // shows it, else the browser.
                                        .on_click(move |event, window, cx| {
                                            super::super::navigation::open_web_link_from(
                                                &browser_url,
                                                event.modifiers(),
                                                None,
                                                window,
                                                cx,
                                            )
                                        }),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    /// The stage's status: unavailable media, or a thread that cannot
    /// show it.
    fn stage_status(&self, cx: &App) -> Option<(SharedString, bool)> {
        if let Some(comments) = &self.comments {
            let comments = comments.read(cx);
            match comments.state() {
                ThreadState::Missing => {
                    return Some(("Original message unavailable.".into(), true));
                }
                ThreadState::Ready if !comments::carries(&comments.rows(), &self.active.url) => {
                    return Some(("Attachment unavailable.".into(), false));
                }
                _ => {}
            }
        }
        if self
            .video
            .as_ref()
            .is_some_and(|video| video.read(cx).failed())
        {
            return Some(("Media unavailable".into(), false));
        }
        None
    }
}

/// `.mediaReviewUnavailable`.
fn unavailable(text: impl Into<SharedString>, c: &buzz_ui::Colors) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .text_body_sm()
        .text_color(c.text_standard)
        .child(text.into())
}

impl Focusable for Review {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Review {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let viewport = window.viewport_size();
        let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
        let padding = if vw < NARROW_WINDOW {
            NARROW_BACKDROP_PADDING
        } else {
            BACKDROP_PADDING
        };
        let (width, height) = (
            (vw - 2. * padding).clamp(0., MAX_SIZE.0),
            (vh - 2. * padding).clamp(0., MAX_SIZE.1),
        );
        let sheet = width <= SHEET_WIDTH;
        let is_video = self.active.kind == AttachmentKind::Video;
        let title: SharedString = if is_video {
            self.active
                .name
                .clone()
                .unwrap_or_else(|| "Video".into())
                .into()
        } else {
            "Image".into()
        };

        // The entrance waits for pixels, then the picture flies.
        let picture = self.picture(window, cx);
        let target = self.picture_bounds(window, cx);
        if !matches!(self.motion, Motion::Closing { .. }) {
            self.drawn = target;
        }
        if let Motion::Pending(since) = self.motion {
            let failed = self.video.as_ref().is_some_and(|v| v.read(cx).failed())
                || matches!(
                    decoded(
                        &Remote {
                            url: self.selected.clone().into(),
                            fallback: None,
                            media: self.media.clone()
                        },
                        window,
                        cx
                    ),
                    Some(Err(_))
                );
            if picture.is_some() && target.is_some() {
                self.motion = Motion::Opening(Instant::now());
            } else if failed || since.elapsed() > Duration::from_secs(3) {
                self.motion = Motion::Settled;
            }
        }
        // A thumbnail that did not repaint once the close began is off
        // screen: fade instead.
        if let Motion::Closing { started, to, .. } = &mut self.motion
            && to.is_some()
            && started.elapsed() > Duration::from_millis(50)
            && let Some(origin) = &self.origin
        {
            *to = tile_since(&origin.url, *started, window);
        }
        let (scrim, chrome, flight): (f32, f32, Option<(Bounds<Pixels>, f32)>) = match &self.motion
        {
            Motion::Pending(_) => (0., 0., None),
            Motion::Opening(started) => {
                let t = progress(*started, Duration::ZERO, FLIGHT);
                if t >= 1. {
                    self.motion = Motion::Settled;
                    (1., 1., None)
                } else {
                    let chrome = progress(*started, CHROME_DELAY, CHROME_IN);
                    let flight = match (self.origin.as_ref(), target) {
                        (Some(origin), Some(target)) => {
                            Some((lerp_bounds(origin.bounds, target, snappy(t)), 1.))
                        }
                        _ => None,
                    };
                    (snappy(t), chrome, flight)
                }
            }
            Motion::Settled => (1., 1., None),
            Motion::Closing { started, to, from } => {
                let t = progress(*started, Duration::ZERO, FLIGHT);
                let fade = progress(*started, Duration::ZERO, FADE_OUT);
                let done = match to {
                    Some(_) => t >= 1.,
                    None => fade >= 1. && t >= 1.,
                };
                if done {
                    window.defer(cx, close);
                }
                let flight = match to {
                    Some(to) => Some((lerp_bounds(*from, *to, snappy(t)), 1.)),
                    None => Some((*from, 1. - fade)),
                };
                (
                    1. - snappy(t),
                    1. - progress(*started, Duration::ZERO, CHROME_OUT),
                    flight,
                )
            }
        };
        let animating = !matches!(self.motion, Motion::Settled);
        if animating {
            window.request_animation_frame();
        }
        let hide_picture = flight.is_some() || matches!(self.motion, Motion::Pending(_));

        let open = self.comments.is_some() && self.comments_open(cx);
        let (level, sliding) = self.sidebar_level(open);
        if sliding {
            window.request_animation_frame();
        } else {
            self.sidebar = None;
        }
        let status = self.stage_status(cx);
        let stage: AnyElement = match (&status, &self.video) {
            (Some((text, retry)), _) => {
                let retry = *retry;
                unavailable(text.clone(), &c)
                    .when(retry, |d| {
                        d.child(Button::new("retry", "Retry").size(ButtonSize::Sm).on_click(
                            cx.listener(|this, _, _, cx| {
                                if let Some(comments) = &this.comments {
                                    comments.read(cx).retry();
                                }
                            }),
                        ))
                    })
                    .into_any_element()
            }
            (None, Some(video)) => div()
                .size_full()
                .when(hide_picture, |d| d.invisible())
                .child(video.clone())
                .into_any_element(),
            (None, None) => self.image_stage(hide_picture, window, cx),
        };
        let stage_bounds = self.stage.clone();
        let stage = div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h(px(if is_video { 224. } else { 160. }))
            .bg(c
                .bg_media_canvas
                .opacity(chrome.max(if hide_picture { 0. } else { 1. })))
            .child(
                canvas(
                    move |bounds, window, _| {
                        if stage_bounds.replace(bounds) != bounds {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0()
                .size_full(),
            )
            .child(stage);
        let sheet_height = (vh * 0.44).clamp(288., 352.);
        // Comments stay mounted while closed.
        if let Some(comments) = &self.comments {
            comments.update(cx, |comments, cx| comments.mount_composer(window, cx));
        }
        let comments = self
            .comments
            .clone()
            .filter(|_| level > 0.)
            .map(|comments| {
                let overlays = comments.update(cx, |comments, cx| comments.overlays(window, cx));
                let panel = div()
                    .flex_none()
                    .overflow_hidden()
                    .opacity(chrome)
                    .border_color(c.border_standard)
                    .map(|d| {
                        if sheet {
                            d.w_full()
                                .h(px(sheet_height * level))
                                .border_t_1()
                                .child(div().w_full().h(px(sheet_height)).child(comments))
                        } else {
                            d.h_full()
                                .w(px(COMMENTS_WIDTH * level))
                                .border_l_1()
                                .child(div().h_full().w(px(COMMENTS_WIDTH)).child(comments))
                        }
                    });
                (panel, overlays)
            });
        let (comments, overlays) = match comments {
            Some((panel, overlays)) => (Some(panel), overlays),
            None => (None, Vec::new()),
        };
        let header = PanelHeader::new()
            .title(title)
            .when(self.comments.is_some(), |header| {
                header.action(
                    IconButton::new("toggle-comments", IconName::SidebarRight)
                        .size(IconButtonSize::Sm)
                        .icon_size(20.)
                        .selected(open)
                        .tooltip(if open {
                            "Hide comments"
                        } else {
                            "Show comments"
                        })
                        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                            this.toggle_comments(!event.is_keyboard(), cx)
                        })),
                )
            })
            .action(
                IconButton::new("close", IconName::X)
                    .size(IconButtonSize::Sm)
                    .icon_size(20.)
                    .tooltip("Close fullscreen viewer")
                    .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                        this.dismiss(!event.is_keyboard(), window, cx)
                    })),
            );
        let closing = matches!(self.motion, Motion::Closing { .. });
        div()
            .id("media-review")
            .key_context("MediaReview")
            .on_key_down(cx.listener(Self::key))
            .occlude()
            .relative()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .items_center()
            .justify_center()
            .text_color(c.text_standard)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| this.dismiss(true, window, cx)),
            )
            .child(div().absolute().inset_0().bg(c.bg_scrim.opacity(scrim)))
            .child(
                div()
                    .id("review-frame")
                    .relative()
                    .w(px(width))
                    .h(px(height))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .bg(c.surface_panel.opacity(chrome))
                    .rounded(px(16.))
                    .border_1()
                    .border_color(c.affordance_subtle_hover.opacity(chrome))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            // Interacting ends the entrance at its final state.
                            if matches!(this.motion, Motion::Opening(_)) {
                                this.motion = Motion::Settled;
                                cx.notify();
                            }
                        }),
                    )
                    .child(div().opacity(chrome).child(header))
                    .child(
                        div()
                            .flex()
                            .when(sheet, |d| d.flex_col())
                            .flex_1()
                            .min_h_0()
                            .child(stage)
                            .children(comments),
                    ),
            )
            .when_some(flight.zip(picture), |this, ((bounds, opacity), picture)| {
                this.child(
                    div()
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .overflow_hidden()
                        .opacity(opacity)
                        .child(img(picture).size_full().object_fit(ObjectFit::Cover)),
                )
            })
            .when(closing, |d| d.cursor_default())
            // The comments' delete and report dialogs.
            .children(overlays)
            // Tab stays inside the viewer (`useModalBoundary`).
            .focus_trap("media-review-trap", &self.focus)
    }
}

/// A local image, decoded once per path.
pub(super) enum LocalImage {}

impl Asset for LocalImage {
    type Source = PathBuf;
    type Output = Result<Arc<Image>, SharedString>;

    #[allow(clippy::manual_async_fn)]
    fn load(path: PathBuf, _: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        async move {
            let bytes = std::fs::read(&path).map_err(|_| SharedString::from("Unreadable file"))?;
            let format = super::image_format(&bytes).ok_or("Not a supported image")?;
            Ok(Arc::new(Image::from_bytes(format, bytes)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn tiles_keep_the_newest_half_once_full() {
        let mut tiles = Tiles::default();
        let start = Instant::now();
        let window = WindowId::from(1);
        for index in 0..=MAX_TILES {
            let at = start + Duration::from_millis(index as u64);
            remember_tile(
                &mut tiles,
                (window, index.to_string()),
                Bounds::default(),
                at,
            );
        }
        assert_eq!(tiles.len(), MAX_TILES / 2);
        assert!(tiles.contains_key(&(window, MAX_TILES.to_string())));
        assert!(!tiles.contains_key(&(window, "0".to_string())));
    }

    #[test]
    fn eases_like_the_reference_curve() {
        assert_eq!(snappy(0.), 0.);
        assert!((snappy(1.) - 1.).abs() < 1e-4);
        // Expo-out: most of the way there early on.
        assert!(snappy(0.25) > 0.8);
        assert!((bezier(0., 0., 1., 1., 0.3) - 0.3).abs() < 1e-3);
    }

    #[test]
    fn fits_pictures_inside_the_stage() {
        let stage = Bounds::new(point(px(10.), px(20.)), size(px(400.), px(200.)));
        let fitted = contained(stage, 1.);
        assert_eq!(fitted.size, size(px(200.), px(200.)));
        assert_eq!(fitted.origin, point(px(110.), px(20.)));
        let half = lerp_bounds(stage, fitted, 0.5);
        assert_eq!(half.size.width, px(300.));
    }
}
