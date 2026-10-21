//! Timecoded replies (`media-timecode.ts`): `⏱ m:ss — text` comments on a
//! video, and the legacy `[m:ss] text` form. Rows show them as a time chip
//! before the body, which seeks the video where the page can.

use super::super::message::Rows;
use super::super::renderers;
use super::media_time;
use buzz_relay_client::fold::MessageRow;
use buzz_ui::{ActiveTheme, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

/// A reply anchored to a moment of the video.
#[derive(Clone, Debug, PartialEq)]
pub struct TimeReply {
    pub seconds: f64,
    /// The time as written, for the chip.
    pub label: String,
    /// The body without its time prefix.
    pub content: String,
}

/// `mediaTimeReply`: `⏱ <m:ss> — <text>`.
pub fn time_reply(seconds: f64, content: &str) -> String {
    format!("⏱ {} — {}", media_time(seconds), content.trim())
}

/// `parseMediaTimeReply`.
pub fn parse(content: &str) -> Option<TimeReply> {
    legacy(content).or_else(|| stamped(content))
}

/// `[1:02] text` or `[1:02:03.5] text`, not followed by `[`, `(` or `:`
/// (which would make it a Markdown link or a label).
fn legacy(content: &str) -> Option<TimeReply> {
    let rest = content.trim_start().strip_prefix('[')?;
    let clock = clock(rest, true)?;
    let after = rest[clock.len..].strip_prefix(']')?;
    if after.starts_with(['[', '(', ':']) {
        return None;
    }
    Some(TimeReply {
        seconds: clock.seconds,
        label: rest[..clock.len].to_string(),
        content: after.trim_start().to_string(),
    })
}

/// `⏱ 1:02 — text`, with an optional emoji variation selector.
fn stamped(content: &str) -> Option<TimeReply> {
    let rest = content.trim_start().strip_prefix('⏱')?;
    let rest = rest.strip_prefix('\u{FE0F}').unwrap_or(rest).trim_start();
    let clock = clock(rest, false)?;
    let after = &rest[clock.len..];
    let spaced = after.trim_start();
    if spaced.len() == after.len() {
        return None;
    }
    let body = spaced.strip_prefix('—')?.trim_start();
    Some(TimeReply {
        seconds: clock.seconds,
        label: rest[..clock.len].to_string(),
        content: body.to_string(),
    })
}

struct Clock {
    /// Bytes of `[h:]m:ss[.fff]`.
    len: usize,
    seconds: f64,
}

/// `[h{1,2}:]m{1,2}:ss`, plus `.f{1,3}` when `fraction` is allowed.
/// Minutes under an hour mark and seconds stay below 60.
fn clock(text: &str, fraction: bool) -> Option<Clock> {
    let digits = |from: usize| text[from..].bytes().take_while(u8::is_ascii_digit).count();
    let mut groups = vec![(0, digits(0))];
    while groups.len() < 3 {
        let (start, len) = *groups.last().expect("one group");
        let next = start + len;
        if len == 0 || text.as_bytes().get(next) != Some(&b':') {
            break;
        }
        groups.push((next + 1, digits(next + 1)));
    }
    let (hours, minutes, seconds) = match groups.as_slice() {
        [h, m, s] => (Some(*h), *m, *s),
        [m, s] => (None, *m, *s),
        _ => return None,
    };
    if hours.is_some_and(|(_, len)| !(1..=2).contains(&len))
        || !(1..=2).contains(&minutes.1)
        || seconds.1 != 2
    {
        return None;
    }
    let mut len = seconds.0 + 2;
    let mut whole: f64 = text[seconds.0..len].parse().ok()?;
    if fraction && text.as_bytes().get(len) == Some(&b'.') {
        let places = digits(len + 1);
        if (1..=3).contains(&places) {
            whole = text[seconds.0..len + 1 + places].parse().ok()?;
            len += 1 + places;
        }
    }
    let read = |(start, len): (usize, usize)| text[start..start + len].parse::<f64>().ok();
    let minutes_value = read(minutes)?;
    let hours_value = match hours {
        Some(hours) => Some(read(hours)?),
        None => None,
    };
    if whole >= 60. || (hours_value.is_some() && minutes_value >= 60.) {
        return None;
    }
    let value = hours_value.unwrap_or(0.) * 3600. + minutes_value * 60. + whole;
    value.is_finite().then_some(Clock {
        len,
        seconds: value,
    })
}

/// A row's text: its Markdown body, after a time chip when it is a
/// timecoded reply. `None` when there is nothing to show.
pub fn body(row: &Arc<MessageRow>, rows: &Rows, cx: &App) -> Option<AnyElement> {
    // A renderer that takes the row draws it whole, time prefix and all.
    if let Some(custom) = renderers::custom_body(row, &rows.directory, cx) {
        return Some(custom);
    }
    let reply = row.diff.is_none().then(|| parse(&row.content)).flatten();
    let Some(reply) = reply else {
        return (!row.content.trim().is_empty() || row.diff.is_some())
            .then(|| crate::ui::markdown::body(row, &rows.directory, cx));
    };
    // The body is a suffix of the content; seams move with it.
    let cut = row.content.len() - reply.content.len();
    let shown = Arc::new(MessageRow {
        content: reply.content.clone(),
        attachment_seams: row
            .attachment_seams
            .iter()
            .filter(|seam| **seam >= cut)
            .map(|seam| seam - cut)
            .collect(),
        ..(**row).clone()
    });
    let text = (!shown.content.trim().is_empty())
        .then(|| crate::ui::markdown::body(&shown, &rows.directory, cx));
    let chip = chip(row, &reply, rows, cx);
    let media = &rows.media;
    Some(if media.inline_time {
        // Viewer comments keep the chip on the first line of text.
        div()
            .flex()
            .items_start()
            .child(chip)
            .when_some(text, |d, text| {
                d.child(div().flex_1().min_w_0().child(text))
            })
            .into_any_element()
    } else {
        div()
            .flex()
            .flex_col()
            .items_start()
            .child(chip)
            .children(text)
            .into_any_element()
    })
}

/// `.mediaTimeLink`: the time in a mono chip, a button where the page can
/// seek to it.
fn chip(row: &MessageRow, reply: &TimeReply, rows: &Rows, cx: &App) -> AnyElement {
    let c = cx.colors();
    let dark_viewer = rows.media.inline_time;
    let chip = div()
        .id(SharedString::from(format!("time-chip-{}", row.id.to_hex())))
        .flex_none()
        .flex()
        .items_center()
        .min_h(px(20.))
        .mr(px(6.))
        .px(px(6.))
        .rounded(buzz_ui::tokens::radius::CHIP)
        .bg(if dark_viewer {
            c.affordance_subtle_hover
        } else {
            c.affordance_selected
        })
        .text_color(c.text_standard)
        .text_mono()
        .font_features(super::tabular())
        .child(reply.label.clone());
    match rows.media.on_time.clone() {
        Some(on_time) => {
            let seconds = reply.seconds;
            chip.cursor_pointer()
                .hover(|s| s.bg(c.affordance_subtle_hover))
                .on_click(move |_, window, cx| on_time(seconds, window, cx))
                .into_any_element()
        }
        None => chip.into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn reply(content: &str) -> Option<(f64, String, String)> {
        parse(content).map(|r| (r.seconds, r.label, r.content))
    }

    #[test]
    fn parses_stamped_replies() {
        assert_eq!(
            reply("⏱ 0:05 — Nice cut"),
            Some((5., "0:05".into(), "Nice cut".into()))
        );
        assert_eq!(
            reply("  ⏱\u{FE0F} 1:02:05 —\nline"),
            Some((3725., "1:02:05".into(), "line".into()))
        );
        assert_eq!(
            reply("⏱ 12:34 —"),
            Some((754., "12:34".into(), String::new()))
        );
        assert_eq!(reply("⏱ 0:60 — x"), None);
        assert_eq!(reply("⏱ 1:60:00 — x"), None);
        assert_eq!(reply("⏱ 0:05— x"), None);
        assert_eq!(reply("⏱ 0:5 — x"), None);
        assert_eq!(reply("⏱ 123:45 — x"), None);
        assert_eq!(reply("⏱ 0:055 — x"), None);
        assert_eq!(reply("0:05 — x"), None);
    }

    #[test]
    fn parses_legacy_brackets() {
        assert_eq!(
            reply("[1:23] hello"),
            Some((83., "1:23".into(), "hello".into()))
        );
        assert_eq!(
            reply("[0:01:02.5]x"),
            Some((62.5, "0:01:02.5".into(), "x".into()))
        );
        assert_eq!(reply("[1:23](https://x)"), None);
        assert_eq!(reply("[1:23]: label"), None);
        assert_eq!(reply("[1:23][ref]"), None);
        assert_eq!(reply("[1:75] x"), None);
        assert_eq!(reply("[1:70:00] x"), None);
        // An hour field longer than any clip is not a time.
        assert_eq!(reply("[9999999:00:00] x"), None);
        assert_eq!(
            reply("[99:00:00] x"),
            Some((356400., "99:00:00".into(), "x".into()))
        );
        // Seventy minutes is fine without an hour field.
        assert_eq!(
            reply("[70:00] x"),
            Some((4200., "70:00".into(), "x".into()))
        );
    }

    #[test]
    fn writes_replies_with_reference_times() {
        assert_eq!(time_reply(5.2, " Nice cut "), "⏱ 0:05 — Nice cut");
        assert_eq!(time_reply(3725., "x"), "⏱ 1:02:05 — x");
        let written = time_reply(3725., "x");
        assert_eq!(parse(&written).map(|r| r.seconds), Some(3725.));
    }
}
