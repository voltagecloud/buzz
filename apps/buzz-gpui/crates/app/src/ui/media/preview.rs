//! The full-width image preview (`MediaAttachment` image mode), for
//! surfaces outside the timeline such as GitHub bodies: the image at its
//! own aspect ratio, `min(32rem, 100%, min(28rem, 56vh) × ratio)` wide,
//! 16/9 until it loads, opening the viewer on click.

use super::{Lazy, Remote, decoded};
use buzz_relay_client::Media;
use buzz_ui::{ActiveTheme, Icon, IconName, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

pub type Open = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageStatus {
    Loading,
    Loaded,
    Failed,
}

fn remote(url: &str, media: &Media) -> Remote {
    Remote {
        url: url.to_string().into(),
        fallback: None,
        media: media.clone(),
    }
}

/// Where the image at `url` is in loading. Reading it starts the load.
pub fn image_status(url: &str, media: &Media, window: &mut Window, cx: &mut App) -> ImageStatus {
    match decoded(&remote(url, media), window, cx) {
        None => ImageStatus::Loading,
        Some(Ok(_)) => ImageStatus::Loaded,
        Some(Err(_)) => ImageStatus::Failed,
    }
}

/// How an [`attachment_unavailable`] note looks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// `.attachmentUnavailable`.
    Note,
    /// `.audioUnavailable`: warning colours and an icon.
    Warning,
}

/// A bordered note in place of the media, as wide as its text.
pub fn attachment_unavailable(text: &'static str, tone: Unavailable, c: &buzz_ui::Colors) -> Div {
    let warning = tone == Unavailable::Warning;
    div().flex().mt(px(8.)).child(
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(12.))
            .py(px(8.))
            .border_1()
            .border_color(if warning {
                c.border_warning
            } else {
                c.border_standard
            })
            .rounded(px(12.))
            .bg(if warning {
                c.affordance_warning
            } else {
                c.affordance_subtle
            })
            .text_body_sm()
            .text_color(if warning {
                c.text_warning
            } else {
                c.text_standard
            })
            .when(warning, |d| {
                d.child(Icon::new(IconName::WarningCircle).size(16.))
            })
            .child(text),
    )
}

/// The preview's width for an image of `ratio` (width / height) in a
/// window `viewport` tall.
pub fn preview_width(ratio: f32, viewport: f32) -> f32 {
    (448f32.min(0.56 * viewport) * ratio).min(512.)
}

/// An image preview with `description` as its label. `open` shows the
/// viewer.
pub fn image_preview(
    id: impl Into<ElementId>,
    url: &str,
    description: SharedString,
    media: &Media,
    open: Open,
) -> impl IntoElement {
    let remote = remote(url, media);
    Lazy::new(id, move |window, cx| {
        let c = *cx.colors();
        let image = decoded(&remote, window, cx);
        let image = match image {
            Some(Err(_)) => {
                return attachment_unavailable("Image unavailable", Unavailable::Note, &c)
                    .into_any_element();
            }
            Some(Ok(image)) => Some(image),
            None => None,
        };
        let ratio = image
            .as_ref()
            .map(|image| image.size(0))
            .filter(|size| size.width.0 > 0 && size.height.0 > 0)
            .map_or(16. / 9., |size| size.width.0 as f32 / size.height.0 as f32);
        let width = preview_width(ratio, f32::from(window.viewport_size().height));
        div()
            .id("image-preview")
            .relative()
            .w(px(width))
            .max_w_full()
            .my(px(4.))
            .cursor(CursorStyle::PointingHand)
            .on_click(move |_, window, cx| open(window, cx))
            .tooltip(buzz_ui::Tooltip::text(description.clone()))
            .child(
                // The height follows the width through the ratio, also
                // when `max-width` narrows it.
                div().w_full().pt(relative(1. / ratio)),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(16.))
                    .overflow_hidden()
                    .bg(c.surface_inset)
                    .when_some(image, |this, image| {
                        this.child(
                            img(image)
                                .absolute()
                                .inset_0()
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                    }),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(16.))
                    .border_1()
                    .border_color(c.border_standard),
            )
            .into_any_element()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn sizes_previews_like_the_media_preview() {
        // Wide images fill 32rem; tall ones are limited by 28rem or 56vh.
        assert_eq!(preview_width(16. / 9., 1000.), 512.);
        assert_eq!(preview_width(0.5, 1000.), 224.);
        assert_eq!(preview_width(0.5, 400.), 112.);
    }
}
