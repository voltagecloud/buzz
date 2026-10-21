//! Attachments under a message (ui.md §4.7): image strips, inline video,
//! audio and voice notes, and file cards. Message rows call
//! [`attachments`]; everything about how media renders lives here.

use super::super::message::Rows;
use super::super::renderers;
use super::Lazy;
use super::review::{open_review, tile_hidden, track_tile};
use super::video::OpenReview;
use buzz_relay_client::fold::{Attachment, AttachmentKind, MessageRow};
use buzz_relay_client::{EventId, Media, ThreadSnapshot};
use buzz_ui::{ActiveTheme, Icon, IconName, Tooltip, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// Seeks a page's video to a time chip's seconds.
pub type OnTime = Rc<dyn Fn(f64, &mut Window, &mut App)>;
/// Opens an attachment of a row in the viewer, at a time for videos.
pub type OnOpen = Rc<dyn Fn(&Arc<MessageRow>, &Attachment, f64, &mut Window, &mut App)>;

/// How a page's rows treat media, beyond the timeline default.
#[derive(Clone, Default)]
pub struct RowMedia {
    /// Time chips are buttons calling this (`onMediaTime`); without it
    /// they are labels.
    pub on_time: Option<OnTime>,
    /// Replaces opening a new viewer (`onOpenMediaReview`).
    pub on_open: Option<OnOpen>,
    /// A seek for one row's inline video: `(row, seconds, request)`. A
    /// new request number seeks again to the same time.
    pub seek: Option<(EventId, f64, u64)>,
    /// The viewer's comments: chips sit on the text's first line.
    pub inline_time: bool,
}

/// Threads and files the maps below remember at most.
const REMEMBERED: usize = 64;

thread_local! {
    /// Each thread's latest time chip seek: `(seconds, request)`. Request
    /// numbers count across threads, so forgetting one cannot reuse them.
    static THREAD_SEEKS: RefCell<HashMap<EventId, (f64, u64)>> = RefCell::default();
    static NEXT_SEEK: Cell<u64> = const { Cell::new(0) };
    /// Files whose last download failed, for the card's alert.
    static FAILED_DOWNLOADS: RefCell<HashSet<String>> = RefCell::default();
}

/// Records a thread's seek, forgetting the oldest beyond [`REMEMBERED`].
fn remember_seek(seeks: &mut HashMap<EventId, (f64, u64)>, root: EventId, seconds: f64) {
    let request = NEXT_SEEK.with(|next| {
        next.set(next.get() + 1);
        next.get()
    });
    seeks.insert(root, (seconds, request));
    if seeks.len() > REMEMBERED
        && let Some(oldest) = seeks.iter().min_by_key(|(_, (_, request))| *request)
    {
        let oldest = *oldest.0;
        seeks.remove(&oldest);
    }
}

/// The thread panel's media handling (`ThreadPanel`): with exactly one
/// video in the thread, time chips seek it. The root's video plays in
/// place from the time; a reply's opens in the viewer there.
pub fn thread_media(snapshot: &ThreadSnapshot, media: &Media) -> RowMedia {
    let root = snapshot.root.clone();
    let rows: Vec<Arc<MessageRow>> = root
        .iter()
        .chain(snapshot.target.iter())
        .chain(snapshot.replies.iter())
        .cloned()
        .collect();
    let mut videos: Vec<&str> = rows
        .iter()
        .flat_map(|row| row.attachments.iter())
        .filter(|a| a.kind == AttachmentKind::Video)
        .map(|a| a.url.as_str())
        .collect();
    videos.sort_unstable();
    videos.dedup();
    let owner = (videos.len() == 1)
        .then(|| {
            rows.iter()
                .find(|row| {
                    row.attachments
                        .iter()
                        .any(|a| a.kind == AttachmentKind::Video)
                })
                .cloned()
        })
        .flatten();
    let seek = root.as_ref().and_then(|root| {
        THREAD_SEEKS
            .with_borrow(|seeks| seeks.get(&root.id).copied())
            .map(|(seconds, request)| (root.id, seconds, request))
    });
    let on_time = owner.zip(root).map(|(owner, root)| {
        let media = media.clone();
        Rc::new(move |seconds: f64, window: &mut Window, cx: &mut App| {
            if owner.id != root.id {
                // A reply's video may sit in a collapsed branch: open it.
                if let Some(video) = owner
                    .attachments
                    .iter()
                    .find(|a| a.kind == AttachmentKind::Video)
                {
                    open_review(&owner, &video.url, &media, seconds, None, window, cx);
                }
                return;
            }
            THREAD_SEEKS.with_borrow_mut(|seeks| remember_seek(seeks, root.id, seconds));
            cx.refresh_windows();
        }) as OnTime
    });
    RowMedia {
        on_time,
        on_open: None,
        seek,
        inline_time: false,
    }
}

/// Attachments of one kind in a row, with their index in the message.
type Members<'a> = Vec<(usize, &'a Attachment)>;

/// Attachments in send order; adjacent images share a strip and adjacent
/// files a group (§4.7). Videos and audio stand alone.
pub fn attachments(row: &Arc<MessageRow>, rows: &Rc<Rows>, cx: &App) -> impl IntoElement {
    let mut groups: Vec<(AttachmentKind, Members)> = Vec::new();
    for (index, attachment) in row.attachments.iter().enumerate() {
        let kind = attachment.kind;
        match groups.last_mut() {
            Some((group, members)) if *group == kind => members.push((index, attachment)),
            _ => groups.push((kind, vec![(index, attachment)])),
        }
    }
    let c = *cx.colors();
    let media = &rows.directory.media;
    div()
        .flex()
        .flex_col()
        .children(groups.into_iter().map(|(kind, members)| {
            if kind == AttachmentKind::Video {
                div()
                    .flex()
                    .flex_col()
                    .children(members.into_iter().map(|(index, attachment)| {
                        if attachment.insecure {
                            return file_tile(row, index, attachment, rows, &c);
                        }
                        let seek = rows
                            .media
                            .seek
                            .filter(|(id, _, _)| *id == row.id)
                            .map(|(_, seconds, request)| (seconds, request));
                        let (target, item, owner) = (row.clone(), attachment.clone(), rows.clone());
                        let open: OpenReview = Rc::new(move |seconds, inline, window, cx| {
                            match &owner.media.on_open {
                                Some(on_open) => on_open(&target, &item, seconds, window, cx),
                                None => open_review(
                                    &target,
                                    &item.url,
                                    &owner.directory.media,
                                    seconds,
                                    Some(inline),
                                    window,
                                    cx,
                                ),
                            }
                        });
                        div()
                            .relative()
                            .flex()
                            .when(tile_hidden(&attachment.url, cx), |d| d.opacity(0.))
                            .child(super::video::video_preview(
                                SharedString::from(format!("video-{}-{index}", row.id.to_hex())),
                                attachment,
                                media,
                                seek,
                                open,
                            ))
                            .into_any_element()
                    }))
                    .into_any_element()
            } else if kind == AttachmentKind::Audio {
                // Only relay media plays inline; other links stay file cards.
                div()
                    .flex()
                    .flex_col()
                    .children(members.into_iter().map(|(index, attachment)| {
                        let id = SharedString::from(format!("audio-{}-{index}", row.id.to_hex()));
                        if attachment.insecure || !media.is_relay_media(&attachment.url) {
                            file_tile(row, index, attachment, rows, &c)
                        } else if let Some(custom) =
                            renderers::attachment(attachment, id.clone(), media, cx)
                        {
                            custom
                        } else {
                            super::audio_attachment(id, attachment, media, cx)
                        }
                    }))
                    .into_any_element()
            } else if kind == AttachmentKind::Image {
                image_group(row, members, rows, cx)
            } else {
                div()
                    .mt(px(8.))
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .children(
                        members
                            .into_iter()
                            .map(|(index, attachment)| file_tile(row, index, attachment, rows, &c)),
                    )
                    .into_any_element()
            }
        }))
}

/// Opens `attachment` in the viewer, or hands it to the page's opener.
fn open(
    rows: &Rows,
    row: &Arc<MessageRow>,
    attachment: &Attachment,
    seconds: f64,
    window: &mut Window,
    cx: &mut App,
) {
    match &rows.media.on_open {
        Some(on_open) => on_open(row, attachment, seconds, window, cx),
        None => open_review(
            row,
            &attachment.url,
            &rows.directory.media,
            seconds,
            None,
            window,
            cx,
        ),
    }
}

/// `.imageGroup`: every image whole, at its own proportions, one under
/// another in attachment order (`.imageList`), then "N images" under
/// groups. Previews take keyboard focus and show a ring for it.
fn image_group(
    row: &Arc<MessageRow>,
    members: Vec<(usize, &Attachment)>,
    rows: &Rc<Rows>,
    cx: &App,
) -> AnyElement {
    let c = *cx.colors();
    let count = members.len();
    let previews: Vec<AnyElement> = members
        .iter()
        .enumerate()
        .map(|(position, (index, attachment))| {
            let label: SharedString = if count > 1 {
                format!("Open image {} of {count}", position + 1).into()
            } else {
                "Open image attachment".into()
            };
            let hidden = tile_hidden(&attachment.url, cx);
            image_preview(row, *index, attachment, rows, label, hidden)
        })
        .collect();
    div()
        .mt(px(8.))
        .min_w_0()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.))
                .p(px(4.))
                .min_w_0()
                .children(previews),
        )
        .when(count > 1, |d| {
            d.child(
                div()
                    .mt(px(4.))
                    .text_caption()
                    .text_color(c.text_subtle)
                    .child(format!("{count} images")),
            )
        })
        .into_any_element()
}

/// An image of `width` × `height` within 360 wide and 320 tall, never
/// enlarged (`AttachmentImage`).
fn preview_size(width: f32, height: f32) -> (f32, f32) {
    let shown = 360f32.min(width).min(320. * width / height);
    (shown, shown * height / width)
}

/// The frame an image takes: its decoded size, else its `imeta dim`
/// (when that reserves at least a pixel each way), else 360 × 320.
fn preview_frame(natural: Option<(f32, f32)>, claimed: Option<(f32, f32)>) -> (f32, f32) {
    if let Some((width, height)) = natural.filter(|(w, h)| *w > 0. && *h > 0.) {
        // Exceptionally thin originals keep a pixel each way.
        let (width, height) = preview_size(width, height);
        return (width.max(1.), height.max(1.));
    }
    claimed
        .filter(|(w, h)| *w > 0. && *h > 0.)
        .map(|(w, h)| preview_size(w, h))
        .filter(|(w, h)| w.is_finite() && h.is_finite() && *w >= 1. && *h >= 1.)
        .unwrap_or((360., 320.))
}

/// One image, whole (`object-fit: contain`): it reserves its frame while
/// loading, with the blurhash or the inset fill, then follows the
/// decoded image's proportions with no backing. A failed decode keeps
/// the loading preview; media that cannot load at all says so in place.
fn image_preview(
    row: &Arc<MessageRow>,
    index: usize,
    attachment: &Attachment,
    rows: &Rc<Rows>,
    label: SharedString,
    hidden: bool,
) -> AnyElement {
    let id = SharedString::from(format!("image-{}-{index}", row.id.to_hex()));
    let media = rows.directory.media.clone();
    let remote = super::Remote {
        url: attachment.url.clone().into(),
        fallback: None,
        media: media.clone(),
    };
    let claimed = attachment
        .dimensions
        .map(|d| (d.width as f32, d.height as f32));
    let hash = attachment.blurhash.clone().map(SharedString::from);
    let (row, item, rows) = (row.clone(), attachment.clone(), rows.clone());
    Lazy::new(id.clone(), move |window, cx| {
        let c = *cx.colors();
        let insecure = item.insecure;
        let image = (!insecure)
            .then(|| super::decoded(&remote, window, cx).and_then(Result::ok))
            .flatten();
        let natural = image.as_ref().map(|image| {
            let size = image.size(0);
            (size.width.0 as f32, size.height.0 as f32)
        });
        let (width, height) = preview_frame(natural, claimed);
        let ready = image.is_some();
        let blur = (!ready && !insecure)
            .then(|| {
                hash.as_ref()
                    .and_then(|hash| window.use_asset::<super::Blurhash>(hash, cx).flatten())
            })
            .flatten();
        let frame = div()
            .id(id.clone())
            .relative()
            .flex_none()
            .w(px(width))
            .max_w_full()
            .min_w(px(1.))
            .my(px(4.))
            // The height follows the width, also when the row narrows it.
            .child(div().w_full().pt(relative(height / width)));
        if insecure {
            return frame
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(4.))
                        .rounded(px(16.))
                        .overflow_hidden()
                        .bg(c.surface_inset)
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child("Image unavailable"),
                )
                .into_any_element();
        }
        let focus = window
            .use_keyed_state(SharedString::from(format!("{id}-focus")), cx, |_, cx| {
                cx.focus_handle().tab_stop(true)
            })
            .read(cx)
            .clone();
        let (key_row, key_item, key_rows) = (row.clone(), item.clone(), rows.clone());
        let (click_row, click_item, click_rows) = (row.clone(), item.clone(), rows.clone());
        frame
            .cursor_pointer()
            .track_focus(&focus)
            .role(Role::Link)
            .aria_label(label.clone())
            .rounded(px(16.))
            .focus_visible(|s| s.border_2().border_color(c.border_focus))
            .when(hidden, |d| d.opacity(0.))
            .child(track_tile(&item.url))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(16.))
                    .overflow_hidden()
                    .when(!ready, |d| d.bg(c.surface_inset))
                    .when_some(blur, |d, blur| {
                        d.child(img(blur).absolute().inset_0().size_full().rounded(px(16.)))
                    })
                    .when_some(image, |d, image| {
                        // Pixels clip to the corners themselves.
                        d.child(img(image).absolute().inset_0().size_full().rounded(px(16.)))
                    }),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(16.))
                    .border_1()
                    .border_color(c.border_image),
            )
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    open(&key_rows, &key_row, &key_item, 0., window, cx);
                    cx.stop_propagation();
                }
            })
            .on_click(move |_, window, cx| {
                open(&click_rows, &click_row, &click_item, 0., window, cx)
            })
            .into_any_element()
    })
    .into_any_element()
}

/// Files, and audio that does not play inline (`FileAttachment`): a card
/// that saves relay media to Downloads without opening it, or opens other
/// links. Plain-HTTP media cannot be fetched, so its card only says so.
fn file_tile(
    row: &Arc<MessageRow>,
    index: usize,
    attachment: &Attachment,
    rows: &Rc<Rows>,
    c: &buzz_ui::Colors,
) -> AnyElement {
    let media = rows.directory.media.clone();
    let insecure = attachment.insecure;
    let relay = !insecure && media.is_relay_media(&attachment.url);
    // A file elsewhere opens as its message's links do: in a panel that
    // shows it, else the browser (`onOpenLink(source)`).
    let (mode, origin) = (
        super::super::links::mode(),
        Some(super::super::navigation::LinkOrigin::of(row)),
    );
    let name = attachment
        .name
        .clone()
        .or_else(|| mime_label(attachment.mime.as_deref()))
        .unwrap_or_else(|| "File".into());
    let meta = match attachment.size {
        _ if insecure => "File unavailable".into(),
        Some(size) if size > 0 => file_size(size),
        _ if relay => "Download file".into(),
        _ => "Open file".into(),
    };
    let failed = relay && FAILED_DOWNLOADS.with_borrow(|failed| failed.contains(&attachment.url));
    let (url, download_name) = (attachment.url.clone(), attachment.name.clone());
    let card = div()
        .id(SharedString::from(format!(
            "file-{}-{index}",
            row.id.to_hex()
        )))
        .flex()
        .items_center()
        .gap(px(8.))
        .max_w(px(384.))
        .min_w_0()
        .px(px(12.))
        .py(px(8.))
        .border_1()
        .border_color(c.border_standard)
        .rounded(px(12.))
        .bg(c.affordance_subtle)
        .when(!insecure, |d| {
            d.hover(|s| s.bg(c.affordance_subtle_hover))
                .cursor_pointer()
        })
        .tooltip(Tooltip::text(name.clone()))
        .child(Icon::new(IconName::FileText).size(24.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w_0()
                .child(
                    div()
                        .text_body_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(name),
                )
                .child(
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .truncate()
                        .child(meta),
                ),
        )
        .when(relay, |d| d.child(Icon::new(IconName::Download).size(20.)))
        .on_click(move |event, window, cx| {
            if insecure {
                return;
            }
            if !relay {
                super::super::links::open_web(
                    mode,
                    &url,
                    event.modifiers(),
                    origin.clone(),
                    window,
                    cx,
                );
                return;
            }
            FAILED_DOWNLOADS.with_borrow_mut(|failed| failed.remove(&url));
            cx.refresh_windows();
            let (url, name, media) = (url.clone(), download_name.clone(), media.clone());
            cx.spawn(async move |cx| {
                if super::save(&media, &url, name.as_deref()).await.is_err() {
                    FAILED_DOWNLOADS.with_borrow_mut(|failed| {
                        // An alert forgotten early only hides; start over.
                        if failed.len() >= REMEMBERED {
                            failed.clear();
                        }
                        failed.insert(url)
                    });
                    cx.update(|cx| cx.refresh_windows());
                }
            })
            .detach();
        });
    if !failed {
        return card.into_any_element();
    }
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.))
        .child(card)
        .child(
            div()
                .text_body_sm()
                .text_color(c.text_standard)
                .child("Download failed"),
        )
        .into_any_element()
}

/// `formatFileSize`.
fn file_size(size: u64) -> String {
    const K: u64 = 1024;
    let round = |unit: u64| (size as f64 / unit as f64).round() as u64;
    match size {
        s if s < K => format!("{s} B"),
        s if s < K * K => format!("{} KB", round(K)),
        s if s < K * K * K => format!("{} MB", round(K * K)),
        _ => format!("{} GB", round(K * K * K)),
    }
}

fn mime_label(mime: Option<&str>) -> Option<String> {
    let (kind, subtype) = mime?.split_once('/')?;
    if kind.is_empty() || subtype.is_empty() {
        return None;
    }
    if subtype == "pdf" {
        return Some("PDF file".into());
    }
    subtype
        .bytes()
        .all(|b| b.is_ascii_alphanumeric())
        .then(|| format!("{} file", subtype.to_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn images_show_whole_within_360_by_320() {
        // Wide, tall and small images keep their proportions, never grow.
        assert_eq!(preview_frame(Some((1600., 900.)), None), (360., 202.5));
        assert_eq!(preview_frame(Some((900., 1600.)), None), (180., 320.));
        assert_eq!(preview_frame(Some((100., 50.)), None), (100., 50.));
        // Until decoded, `imeta dim` reserves the frame, else 360 × 320.
        assert_eq!(preview_frame(None, Some((800., 800.))), (320., 320.));
        assert_eq!(preview_frame(None, None), (360., 320.));
        // Metadata that would reserve less than a pixel is ignored...
        assert_eq!(preview_frame(None, Some((1., 10000.))), (360., 320.));
        // ...but a decoded thin original keeps a pixel each way.
        assert_eq!(preview_frame(Some((1., 10000.)), None), (1., 320.));
        assert_eq!(preview_frame(Some((10000., 1.)), None), (360., 1.));
    }

    #[test]
    fn thread_seeks_forget_the_oldest_and_never_reuse_requests() {
        let mut seeks = HashMap::new();
        let id = |byte: u8| EventId::from_byte_array([byte; 32]);
        for byte in 0..=REMEMBERED as u8 {
            remember_seek(&mut seeks, id(byte), 1.);
        }
        assert_eq!(seeks.len(), REMEMBERED);
        assert!(!seeks.contains_key(&id(0)));
        let last = seeks[&id(REMEMBERED as u8)].1;
        remember_seek(&mut seeks, id(0), 2.);
        assert!(seeks[&id(0)].1 > last);
    }

    #[test]
    fn formats_file_sizes_like_the_reference() {
        assert_eq!(file_size(900), "900 B");
        assert_eq!(file_size(13 * 1024 + 100), "13 KB");
        assert_eq!(file_size(5 * 1024 * 1024), "5 MB");
        assert_eq!(
            mime_label(Some("application/pdf")).as_deref(),
            Some("PDF file")
        );
        assert_eq!(
            mime_label(Some("text/plain")).as_deref(),
            Some("PLAIN file")
        );
        assert_eq!(mime_label(Some("application/vnd.x+y")), None);
    }
}
