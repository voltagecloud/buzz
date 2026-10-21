//! The "poof" when an attachment is removed (`AttachmentPoof.tsx`): five
//! sprite frames, 80ms each, centred on the remove button, and a quiet
//! "plop" at 0.34 volume. Reduced motion shows only the middle frame,
//! fading over 180ms. The sound is best effort.

use gpui_kit::*;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const FRAMES: [&[u8]; 5] = [
    include_bytes!("../../../../../assets/recipient-removal/poof1@3x.png"),
    include_bytes!("../../../../../assets/recipient-removal/poof2@3x.png"),
    include_bytes!("../../../../../assets/recipient-removal/poof3@3x.png"),
    include_bytes!("../../../../../assets/recipient-removal/poof4@3x.png"),
    include_bytes!("../../../../../assets/recipient-removal/poof5@3x.png"),
];
const PLOP: &[u8] = include_bytes!("../../../../../assets/recipient-removal/plop.m4a");

const DURATION: Duration = Duration::from_millis(400);
const REDUCED: Duration = Duration::from_millis(180);
/// A burst is forgotten after this, as `setTimeout(…, 430)`.
const LIFETIME: Duration = Duration::from_millis(430);

/// One poof, centred on `origin`.
pub struct Burst {
    origin: Point<Pixels>,
    size: Pixels,
    started: Instant,
}

impl Burst {
    /// A burst for a button of `width` around `origin`.
    pub fn new(origin: Point<Pixels>, width: Pixels) -> Self {
        let size = (width * 0.54).max(px(104.)).min(px(190.)) * 0.6375;
        Self {
            origin,
            size,
            started: Instant::now(),
        }
    }

    pub fn done(&self) -> bool {
        self.started.elapsed() >= LIFETIME
    }
}

fn frames() -> &'static [Arc<Image>; 5] {
    static FRAMES_IMAGES: OnceLock<[Arc<Image>; 5]> = OnceLock::new();
    FRAMES_IMAGES.get_or_init(|| {
        FRAMES.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes.to_vec())))
    })
}

/// The visible frame of `burst` now, its scale and opacity.
fn frame(elapsed: Duration, reduce: bool) -> Option<(usize, f32, f32)> {
    if reduce {
        let t = elapsed.as_secs_f32() / REDUCED.as_secs_f32();
        return (t < 1.).then_some((2, 1.01, 0.9 * (1. - t)));
    }
    let t = elapsed.as_secs_f32() / DURATION.as_secs_f32();
    if t >= 1. {
        return None;
    }
    let index = ((t * 5.) as usize).min(4);
    // Each frame shows at its own scale: 0.94, then 1 to 1.03.
    let scale = [0.94, 1., 1.01, 1.02, 1.03][index];
    Some((index, scale, 1.))
}

/// The bursts as a window-wide overlay that never takes the pointer.
pub fn overlay(bursts: &[Burst], window: &mut Window, cx: &App) -> Option<AnyElement> {
    if bursts.is_empty() {
        return None;
    }
    let reduce = cx.reduce_motion();
    let images = frames();
    let mut children = Vec::new();
    for burst in bursts {
        let Some((index, scale, opacity)) = frame(burst.started.elapsed(), reduce) else {
            continue;
        };
        let size = burst.size * scale;
        children.push(
            div()
                .absolute()
                .left(burst.origin.x - size / 2.)
                .top(burst.origin.y - size / 2.)
                .size(size)
                .opacity(opacity)
                .child(
                    img(images[index].clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                ),
        );
    }
    window.request_animation_frame();
    Some(
        deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Window)
                .position(point(px(0.), px(0.)))
                .child(
                    div()
                        .w(window.viewport_size().width)
                        .h(window.viewport_size().height)
                        .children(children),
                ),
        )
        .with_priority(4)
        .into_any_element(),
    )
}

/// The frames, drawn invisibly while there are attachments so they are
/// decoded before a removal needs them (the reference preloads them too).
pub fn preload() -> impl IntoElement {
    div()
        .absolute()
        .size(px(1.))
        .opacity(0.)
        .overflow_hidden()
        .children(frames().iter().map(|frame| img(frame.clone()).size(px(1.))))
}

/// Plays the plop, restarting it if it still plays.
pub fn plop() {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    static PLAYER: std::sync::Mutex<Option<gst::Element>> = std::sync::Mutex::new(None);
    if crate::gst_bundle::init().is_err() {
        return;
    }
    let path = std::env::temp_dir().join("buzz-gpui-plop.m4a");
    if !path.exists() && std::fs::write(&path, PLOP).is_err() {
        return;
    }
    let Ok(uri) = url::Url::from_file_path(&path) else {
        return;
    };
    let Ok(playbin) = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("volume", 0.34f64)
        .build()
    else {
        return;
    };
    // Video is never wanted; a missing audio device stays silent.
    playbin.set_property_from_str("flags", "audio+soft-volume");
    if let Ok(mut current) = PLAYER.lock()
        && let Some(previous) = current.replace(playbin.clone())
    {
        let _ = previous.set_state(gst::State::Null);
    }
    if playbin.set_state(gst::State::Playing).is_err() {
        return;
    }
    let bus = playbin.bus();
    std::thread::spawn(move || {
        if let Some(bus) = bus {
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = playbin.set_state(gst::State::Null);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn plays_five_frames_or_one_fading() {
        assert_eq!(frame(Duration::from_millis(0), false), Some((0, 0.94, 1.)));
        assert_eq!(
            frame(Duration::from_millis(170), false).map(|f| f.0),
            Some(2)
        );
        assert_eq!(
            frame(Duration::from_millis(390), false).map(|f| f.0),
            Some(4)
        );
        assert_eq!(frame(Duration::from_millis(400), false), None);
        assert_eq!(frame(Duration::from_millis(0), true).map(|f| f.0), Some(2));
        assert_eq!(frame(Duration::from_millis(180), true), None);
        let burst = Burst::new(point(px(0.), px(0.)), px(20.));
        assert_eq!(burst.size, px(104. * 0.6375));
    }
}
