//! A composer draft's fullscreen viewer (`ComposerAttachmentViewer.tsx`
//! in `MediaReviewFrame.tsx`): the dark review frame titled with the file
//! name, its picture, video or audio, and with several files, Previous and
//! Next with "n / total" and the arrow keys. A picture flies from its tile
//! and back on close, as sent media does.
//!
//! The viewer follows the draft: a file removed while it shows closes it.
//! A slide keeps playing from the file on this computer after its upload
//! completes; a file already on the relay shows from there.

use super::super::composer::attachments::{AttachmentDraft, DraftFile, Preview};
use super::review::{FLIGHT, contained, lerp_bounds, progress, snappy, tile, tile_since};
use super::video::{Clip, Mode, SKIP_SECONDS, VideoPlayer};
use super::{Remote, decoded};
use buzz_relay_client::fold::{Attachment, AttachmentKind};
use buzz_relay_client::upload::Source;
use buzz_ui::{ActiveTheme, IconButton, IconButtonSize, IconName, PanelHeader, TypeStyled};
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `galleryMotion`: 200ms, 40px, `cubic-bezier(0.19, 1, 0.22, 1)`.
const SLIDE: Duration = Duration::from_millis(200);
const SLIDE_SHIFT: f32 = 40.;
const CHROME_IN: Duration = Duration::from_millis(160);
const CHROME_DELAY: Duration = Duration::from_millis(80);
const CHROME_OUT: Duration = Duration::from_millis(100);
const FADE_OUT: Duration = Duration::from_millis(160);
/// The frame's backdrop padding (12px below 50rem) and largest size.
const BACKDROP_PADDING: f32 = 40.;
const NARROW_BACKDROP_PADDING: f32 = 12.;
const NARROW_WINDOW: f32 = 800.;
const MAX_SIZE: (f32, f32) = (1520., 980.);

/// The key a draft tile's bounds are kept under, for the flight.
pub fn tile_key(id: &str) -> String {
    format!("draft-attachment:{id}")
}

fn gallery_ease(t: f32) -> f32 {
    super::review::bezier(0.19, 1., 0.22, 1., t)
}

enum Motion {
    Opening(Instant),
    Settled,
    Closing {
        started: Instant,
        /// The tile it returns to, or `None` to fade out.
        to: Option<Bounds<Pixels>>,
        from: Option<Bounds<Pixels>>,
    },
}

/// The slide leaving as the viewer moves to another file.
struct Leaving {
    id: String,
    direction: f32,
}

pub struct DraftViewer {
    pub(super) focus: FocusHandle,
    draft: Entity<AttachmentDraft>,
    selected: String,
    /// The tile the viewer opened from, and where it was.
    origin: Option<(String, Bounds<Pixels>)>,
    motion: Motion,
    leaving: Option<Leaving>,
    arriving: Option<(f32, Instant)>,
    /// Each opened file's player, kept until the viewer closes.
    pub(super) players: HashMap<String, Entity<VideoPlayer>>,
    /// Pasted pictures, decoded once.
    images: HashMap<String, Arc<Image>>,
    /// Audio files, read into memory to play under this key.
    sounds: Vec<String>,
    /// Whether each audio file shown was on the relay when its slide
    /// showed: one shown before its upload keeps playing here while its
    /// slide shows (`keepLocal`).
    hosted: HashMap<String, bool>,
    /// The picture's bounds as last drawn, where a close flies from.
    drawn: Option<Bounds<Pixels>>,
    stage: Rc<Cell<Bounds<Pixels>>>,
    _observe: Subscription,
}

impl DraftViewer {
    pub fn new(
        draft: Entity<AttachmentDraft>,
        selected: String,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&draft, |this, draft, cx| {
            // A removed file closes the viewer (`setSelected(undefined)`).
            if !draft.read(cx).items.iter().any(|i| i.id == this.selected) {
                this.motion = Motion::Closing {
                    started: Instant::now() - FLIGHT * 2,
                    to: None,
                    from: None,
                };
            }
            cx.notify();
        });
        let key = tile_key(&selected);
        let origin = (!cx.reduce_motion())
            .then(|| tile(&key, window))
            .flatten()
            .map(|bounds| (key, bounds));
        Self {
            focus: cx.focus_handle().tab_stop(true),
            draft,
            selected,
            origin,
            motion: if cx.reduce_motion() {
                Motion::Settled
            } else {
                Motion::Opening(Instant::now())
            },
            leaving: None,
            arriving: None,
            players: HashMap::new(),
            images: HashMap::new(),
            sounds: Vec::new(),
            hosted: HashMap::new(),
            drawn: None,
            stage: Rc::default(),
            _observe: observe,
        }
    }

    fn items(&self, cx: &App) -> Vec<DraftFile> {
        self.draft.read(cx).items.clone()
    }

    /// Previous (-1) or Next (1).
    fn navigate(&mut self, direction: i32, cx: &mut Context<Self>) {
        let items = self.items(cx);
        let Some(index) = items.iter().position(|item| item.id == self.selected) else {
            return;
        };
        let Some(next) = index
            .checked_add_signed(direction as isize)
            .and_then(|i| items.get(i))
        else {
            return;
        };
        if let Some(player) = self.players.get(&self.selected) {
            player.update(cx, |player, cx| player.pause(cx));
        }
        // A slide shown again starts afresh, from the relay if it is
        // there now (`DraftMedia` mounts anew).
        self.hosted.remove(&next.id);
        let reduce = cx.reduce_motion();
        if !reduce {
            self.leaving = Some(Leaving {
                id: self.selected.clone(),
                direction: direction as f32,
            });
            self.arriving = Some((direction as f32, Instant::now()));
        }
        // Closing now returns to this file's tile.
        self.origin = None;
        self.selected = next.id.clone();
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.motion, Motion::Closing { .. }) {
            return;
        }
        if let Some(player) = self.players.get(&self.selected) {
            player.update(cx, |player, cx| player.pause(cx));
        }
        if cx.reduce_motion() {
            window.defer(cx, super::review::close_preview);
            return;
        }
        let key = tile_key(&self.selected);
        self.motion = Motion::Closing {
            started: Instant::now(),
            to: self.drawn.and(tile(&key, window)),
            from: self.drawn,
        };
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.shift {
            return;
        }
        if matches!(self.motion, Motion::Opening(_)) {
            self.motion = Motion::Settled;
        }
        let several = self.items(cx).len() > 1;
        let player = self.players.get(&self.selected).cloned();
        match (keystroke.key.as_str(), player) {
            ("escape", _) => self.dismiss(window, cx),
            ("left", _) if several => self.navigate(-1, cx),
            ("right", _) if several => self.navigate(1, cx),
            ("left", Some(player)) => player.update(cx, |p, cx| p.seek_by(-SKIP_SECONDS, cx)),
            ("right", Some(player)) => player.update(cx, |p, cx| p.seek_by(SKIP_SECONDS, cx)),
            ("space", Some(player)) if !event.is_held => player.update(cx, |p, cx| p.toggle(cx)),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The decoded picture of an image file: from this computer, else
    /// from the relay. `Some(Err)` when it cannot be shown.
    fn picture(
        &mut self,
        item: &DraftFile,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ()>> {
        match &item.source {
            Source::Bytes(bytes) if bytes.is_empty() => {
                let attachment = item.uploaded.as_ref()?;
                let remote = Remote {
                    url: attachment.url.clone().into(),
                    fallback: None,
                    media: self.draft.read(cx).media().clone(),
                };
                decoded(&remote, window, cx).map(|result| result.map_err(|_| ()))
            }
            Source::Bytes(bytes) => {
                let image = match self.images.get(&item.id) {
                    Some(image) => image.clone(),
                    None => {
                        let format = super::image_format(bytes).ok_or(());
                        let Ok(format) = format else {
                            return Some(Err(()));
                        };
                        let image = Arc::new(Image::from_bytes(format, bytes.to_vec()));
                        self.images.insert(item.id.clone(), image.clone());
                        image
                    }
                };
                image.use_render_image(window, cx).map(Ok)
            }
            Source::Path(path) => match window.use_asset::<super::review::LocalImage>(path, cx)? {
                Ok(image) => image.use_render_image(window, cx).map(Ok),
                Err(_) => Some(Err(())),
            },
        }
    }

    /// The player for a video file, made when it first shows.
    fn player(&mut self, item: &DraftFile, cx: &mut Context<Self>) -> Option<Entity<VideoPlayer>> {
        if let Some(player) = self.players.get(&item.id) {
            return Some(player.clone());
        }
        let clip = match (&item.source, &item.uploaded) {
            (Source::Path(path), _) => Clip::Local(path.clone()),
            (_, Some(uploaded)) => Clip::Remote {
                attachment: attachment(item, uploaded),
                media: self.draft.read(cx).media().clone(),
            },
            _ => return None,
        };
        let player = cx.new(|cx| VideoPlayer::new(clip, Mode::Review, 0., cx));
        cx.observe(&player, |_, _, cx| cx.notify()).detach();
        self.players.insert(item.id.clone(), player.clone());
        Some(player)
    }

    /// An audio file plays from memory under a key of its own, and a
    /// voice note under its own; from the relay if it was uploaded when
    /// its slide first showed.
    fn sound(&mut self, item: &DraftFile, cx: &mut Context<Self>) -> Option<Attachment> {
        let hosted = *self
            .hosted
            .entry(item.id.clone())
            .or_insert(item.uploaded.is_some());
        let uploaded = item.uploaded.as_ref().filter(|_| hosted);
        if let Some(voice) = &item.voice {
            let base = match uploaded {
                Some(uploaded) => attachment(item, uploaded),
                None => local(voice.key.clone(), item),
            };
            return Some(Attachment {
                duration: Some(voice.duration),
                waveform: Some(voice.waveform.clone()),
                voice_note: true,
                ..base
            });
        }
        if let Some(uploaded) = uploaded {
            return Some(attachment(item, uploaded));
        }
        let key = format!("draft-audio:{}", item.id);
        self.hold(&key, item, cx);
        Some(local(key, item))
    }

    /// Holds `item`'s bytes in memory under `key` until the viewer closes.
    fn hold(&mut self, key: &str, item: &DraftFile, cx: &mut Context<Self>) {
        if self.sounds.iter().any(|held| held == key) {
            return;
        }
        self.sounds.push(key.to_owned());
        let (source, local) = (item.source.clone(), key.to_owned());
        let read = cx.background_executor().spawn(async move {
            match source {
                Source::Path(path) => std::fs::read(path).ok().map(Arc::<[u8]>::from),
                Source::Bytes(bytes) => Some(bytes),
            }
        });
        cx.spawn(async move |this, cx| {
            if let Some(bytes) = read.await {
                let _ = this.update(cx, |_, cx| {
                    super::set_local(&local, bytes, cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// An uploaded file a plugin's attachment renderer draws, as
    /// `ComposerAttachmentViewer` offers one by its relay URL. Pictures
    /// and videos show in Buzz's own stage, and a file not yet uploaded
    /// has no preview.
    fn plugin_slide(&mut self, item: &DraftFile, cx: &mut Context<Self>) -> Option<AnyElement> {
        if item.preview() != Preview::File {
            return None;
        }
        let attachment = attachment(item, item.uploaded.as_ref()?);
        if !crate::ui::renderers::takes_attachment(&attachment) {
            return None;
        }
        let media = self.draft.read(cx).media().clone();
        crate::ui::renderers::without_removal(|| {
            crate::ui::renderers::attachment(
                &attachment,
                SharedString::from(format!("draft-file-plugin-{}", item.id)),
                &media,
                cx,
            )
        })
    }

    /// One file's slide (`DraftMedia`).
    fn slide(
        &mut self,
        item: &DraftFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let unavailable = |text: &str| {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .text_body()
                .text_color(c.text_subtle)
                .child(text.to_string())
                .into_any_element()
        };
        if let Some(custom) = self.plugin_slide(item, cx) {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.))
                .child(custom)
                .into_any_element();
        }
        match item.preview() {
            Preview::Image => match self.picture(item, window, cx) {
                Some(Ok(image)) => {
                    // Fitted inside the stage, centred (`object-fit:
                    // contain`).
                    let natural = image.size(0);
                    let aspect = natural.width.0 as f32 / natural.height.0.max(1) as f32;
                    let stage = Bounds::new(point(px(0.), px(0.)), self.stage.get().size);
                    let fitted = contained(stage, aspect);
                    div()
                        .size_full()
                        .relative()
                        .child(
                            img(image)
                                .absolute()
                                .left(fitted.origin.x)
                                .top(fitted.origin.y)
                                .w(fitted.size.width)
                                .h(fitted.size.height),
                        )
                        .into_any_element()
                }
                Some(Err(())) => unavailable("Preview unavailable"),
                None => div().size_full().into_any_element(),
            },
            Preview::Video => match self.player(item, cx) {
                Some(player) if player.read(cx).failed() => unavailable("Preview unavailable"),
                Some(player) => div().size_full().child(player).into_any_element(),
                None => unavailable("Preview unavailable"),
            },
            Preview::Audio => match self.sound(item, cx) {
                Some(attachment) => {
                    let media = self.draft.read(cx).media().clone();
                    let id = SharedString::from(format!("draft-audio-{}", item.id));
                    // A plugin's attachment renderer draws drafts as sent
                    // ones (`ComposerAttachmentViewer`).
                    // The viewer offers no removal (no `onRemove`).
                    let custom = crate::ui::renderers::without_removal(|| {
                        crate::ui::renderers::attachment(
                            &attachment,
                            SharedString::from(format!("draft-audio-plugin-{}", item.id)),
                            &media,
                            cx,
                        )
                    });
                    let player = match custom {
                        Some(custom) => custom,
                        None if attachment.voice_note => {
                            super::voice_draft(id, &attachment, &media, cx)
                        }
                        None => super::audio_attachment(id, &attachment, &media, cx),
                    };
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(24.))
                        .child(div().w(px(336.)).max_w_full().child(player))
                        .into_any_element()
                }
                None => unavailable("Preview unavailable"),
            },
            Preview::File => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .text_body()
                .text_color(c.text_subtle)
                .child(item.name.clone())
                .child("No preview available for this file type.")
                .into_any_element(),
        }
    }
}

/// An audio file playing from memory under `key`.
fn local(key: String, item: &DraftFile) -> Attachment {
    Attachment {
        url: key,
        kind: AttachmentKind::Audio,
        mime: Some(item.mime.clone()),
        size: Some(item.size),
        name: Some(item.name.clone()),
        duration: None,
        waveform: None,
        voice_note: false,
        dimensions: None,
        blurhash: None,
        preview_url: None,
        insecure: false,
    }
}

/// A file already on the relay.
fn attachment(item: &DraftFile, uploaded: &buzz_relay_client::Attachment) -> Attachment {
    let kind = match item.preview() {
        Preview::Image => AttachmentKind::Image,
        Preview::Video => AttachmentKind::Video,
        Preview::Audio => AttachmentKind::Audio,
        Preview::File => AttachmentKind::File,
    };
    Attachment {
        url: uploaded.url.clone(),
        kind,
        mime: Some(uploaded.mime.clone()),
        size: Some(uploaded.size),
        name: Some(uploaded.name.clone()),
        ..local(String::new(), item)
    }
}

impl Focusable for DraftViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DraftViewer {
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
        let items = self.items(cx);
        let index = items.iter().position(|item| item.id == self.selected);
        let Some(index) = index else {
            window.defer(cx, super::review::close_preview);
            return div().into_any_element();
        };
        let item = items[index].clone();

        // The picture's fitted bounds in the stage, where it flies to.
        let stage_bounds = self.stage.get();
        let picture = (item.preview() == Preview::Image)
            .then(|| self.picture(&item, window, cx))
            .flatten()
            .and_then(Result::ok);
        let target = picture.as_ref().and_then(|picture| {
            let natural = picture.size(0);
            let aspect = natural.width.0 as f32 / natural.height.0.max(1) as f32;
            (stage_bounds.size.width > px(0.)).then(|| contained(stage_bounds, aspect))
        });
        if !matches!(self.motion, Motion::Closing { .. }) {
            self.drawn = target;
        }
        if let Motion::Closing { started, to, .. } = &mut self.motion
            && to.is_some()
            && started.elapsed() > Duration::from_millis(50)
        {
            *to = tile_since(&tile_key(&self.selected), *started, window);
        }
        let (scrim, chrome, flight) = match &self.motion {
            Motion::Opening(started) => {
                let t = progress(*started, Duration::ZERO, FLIGHT);
                if t >= 1. {
                    self.motion = Motion::Settled;
                    (1., 1., None)
                } else {
                    let chrome = progress(*started, CHROME_DELAY, CHROME_IN);
                    let flight = match (&self.origin, target) {
                        (Some((_, from)), Some(to)) => {
                            Some((lerp_bounds(*from, to, snappy(t)), 1.))
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
                    window.defer(cx, super::review::close_preview);
                }
                let flight = match (to, from) {
                    (Some(to), Some(from)) => Some((lerp_bounds(*from, *to, snappy(t)), 1.)),
                    (None, Some(from)) => Some((*from, 1. - fade)),
                    _ => None,
                };
                (
                    1. - snappy(t),
                    1. - progress(*started, Duration::ZERO, CHROME_OUT),
                    flight,
                )
            }
        };
        if !matches!(self.motion, Motion::Settled) {
            window.request_animation_frame();
        }
        let hide_picture = flight.is_some();

        // The arriving slide moves in as the leaving one moves out.
        let slide_t = self
            .arriving
            .map(|(_, started)| gallery_ease(progress(started, Duration::ZERO, SLIDE)));
        if slide_t.is_some_and(|t| t >= 1.) {
            self.arriving = None;
            self.leaving = None;
        } else if slide_t.is_some() {
            window.request_animation_frame();
        }
        let current = self.slide(&item, window, cx);
        let arriving = self.arriving.map(|(direction, _)| direction);
        let leaving = self.leaving.as_ref().map(|l| (l.id.clone(), l.direction));
        let leaving = leaving.and_then(|(id, direction)| {
            let item = items.iter().find(|item| item.id == id)?.clone();
            Some((self.slide(&item, window, cx), direction))
        });
        let t = slide_t.unwrap_or(1.);
        let layer = |content: AnyElement, shift: f32, opacity: f32| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(shift))
                .w_full()
                .opacity(opacity)
                .child(content)
        };
        let stage_cell = self.stage.clone();
        let stage =
            div()
                .relative()
                .flex_1()
                .min_w_0()
                .min_h(px(160.))
                .overflow_hidden()
                .bg(c
                    .bg_media_canvas
                    .opacity(chrome.max(if hide_picture { 0. } else { 1. })))
                .child(
                    canvas(
                        move |bounds, window, _| {
                            if stage_cell.replace(bounds) != bounds {
                                window.request_animation_frame();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0()
                    .size_full(),
                )
                .children(leaving.map(|(content, direction)| {
                    layer(content, -direction * SLIDE_SHIFT * t, 1. - t)
                }))
                .child(
                    layer(
                        current,
                        arriving.map_or(0., |direction| direction * SLIDE_SHIFT * (1. - t)),
                        t,
                    )
                    .when(hide_picture, |d| d.invisible()),
                );

        let several = items.len() > 1;
        let mut header = PanelHeader::new().title(item.name.clone());
        if several {
            header = header
                .action(
                    IconButton::new("draft-previous", IconName::CaretLeft)
                        .size(IconButtonSize::Sm)
                        .icon_size(20.)
                        .tooltip("Previous attachment")
                        .disabled(index == 0)
                        .on_click(cx.listener(|this, _, _, cx| this.navigate(-1, cx))),
                )
                .action(div().text_body().text_color(c.text_standard).child(format!(
                    "{} / {}",
                    index + 1,
                    items.len()
                )))
                .action(
                    IconButton::new("draft-next", IconName::CaretRight)
                        .size(IconButtonSize::Sm)
                        .icon_size(20.)
                        .tooltip("Next attachment")
                        .disabled(index + 1 == items.len())
                        .on_click(cx.listener(|this, _, _, cx| this.navigate(1, cx))),
                );
        }
        let header = header.action(
            IconButton::new("draft-close", IconName::X)
                .size(IconButtonSize::Sm)
                .icon_size(20.)
                .tooltip("Close fullscreen viewer")
                .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
        );
        div()
            .id("draft-viewer")
            .key_context("MediaReview")
            .track_focus(&self.focus)
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
                cx.listener(|this, _: &MouseDownEvent, window, cx| this.dismiss(window, cx)),
            )
            .child(div().absolute().inset_0().bg(c.bg_scrim.opacity(scrim)))
            .child(
                div()
                    .id("draft-viewer-frame")
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
                            if matches!(this.motion, Motion::Opening(_)) {
                                this.motion = Motion::Settled;
                                cx.notify();
                            }
                        }),
                    )
                    .child(div().opacity(chrome).child(header))
                    .child(div().flex().flex_1().min_h_0().child(stage)),
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
            .focus_trap("draft-viewer-trap", &self.focus)
            .into_any_element()
    }
}

impl DraftViewer {
    /// Frees what the viewer holds once it closes: its players' frames
    /// (only the window can drop them) and audio read into memory.
    pub(super) fn release(&mut self, window: &mut Window, cx: &mut App) {
        for player in self.players.values() {
            for image in player.update(cx, |player, _| player.take_frames()) {
                let _ = window.drop_image(image);
            }
        }
        for key in self.sounds.drain(..) {
            super::forget_local(&key, cx);
        }
    }

    /// Whether the tile of `id` hides because its picture is in flight.
    pub fn flying(&self, id: &str) -> bool {
        self.selected == id
            && match &self.motion {
                Motion::Opening(_) => self.origin.is_some(),
                Motion::Closing { to, .. } => to.is_some(),
                Motion::Settled => false,
            }
    }
}
