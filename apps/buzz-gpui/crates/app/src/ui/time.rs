//! Timestamp wording from buzz-app's `src/shared/datetime.ts` (en-US).

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use gpui_kit::{Context, Task};
use std::time::Duration;

fn local(seconds: u64) -> DateTime<Local> {
    Local
        .timestamp_opt(seconds as i64, 0)
        .single()
        .unwrap_or_else(Local::now)
}

fn days_ago(date: NaiveDate, today: NaiveDate) -> i64 {
    (today - date).num_days()
}

fn clock(time: &DateTime<Local>) -> String {
    time.format("%-I:%M %p").to_string()
}

/// Byline time: "2:34 PM", "Yesterday at 2:34 PM", "Monday at 2:34 PM",
/// "Sat, Jun 20 at 2:34 PM", "Jun 20, 2025 at 2:34 PM".
pub fn byline(seconds: u64) -> String {
    byline_at(seconds, Local::now())
}

/// `seconds` as unix time, for a "now" other than the clock's.
pub fn at(seconds: u64) -> DateTime<Local> {
    local(seconds)
}

pub fn byline_at(seconds: u64, now: DateTime<Local>) -> String {
    let time = local(seconds);
    let (date, today) = (time.date_naive(), now.date_naive());
    match days_ago(date, today) {
        0 => clock(&time),
        1 => format!("Yesterday at {}", clock(&time)),
        2..=6 => format!("{} at {}", time.format("%A"), clock(&time)),
        _ if date.year() == today.year() => {
            format!("{} at {}", time.format("%a, %b %-d"), clock(&time))
        }
        _ => format!("{} at {}", time.format("%b %-d, %Y"), clock(&time)),
    }
}

/// A list row's time (`formatItemTimestamp`): "2:34 PM", "Yesterday",
/// "Monday", "Sat, Jun 20", "Jun 20, 2025".
pub fn item_at(seconds: u64, now: DateTime<Local>) -> String {
    let time = local(seconds);
    let (date, today) = (time.date_naive(), now.date_naive());
    match days_ago(date, today) {
        0 => clock(&time),
        1 => "Yesterday".into(),
        2..=6 => time.format("%A").to_string(),
        _ if date.year() == today.year() => time.format("%a, %b %-d").to_string(),
        _ => time.format("%b %-d, %Y").to_string(),
    }
}

/// Continuation gutter: "2:34", without the day period.
pub fn compact(seconds: u64) -> String {
    local(seconds).format("%-I:%M").to_string()
}

/// Day divider: "Today", "Yesterday", "Monday", "Saturday, June 20",
/// "June 20, 2025".
pub fn day(seconds: u64) -> String {
    day_at(seconds, Local::now())
}

pub fn day_at(seconds: u64, now: DateTime<Local>) -> String {
    let time = local(seconds);
    let (date, today) = (time.date_naive(), now.date_naive());
    match days_ago(date, today) {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => time.format("%A").to_string(),
        _ if date.year() == today.year() => time.format("%A, %B %-d").to_string(),
        _ => time.format("%B %-d, %Y").to_string(),
    }
}

/// The local day of the month, for the day divider's calendar.
pub fn day_of_month(seconds: u64) -> u32 {
    local(seconds).day()
}

/// Whether two times fall on the same local calendar day.
pub fn same_day(a: u64, b: u64) -> bool {
    local(a).date_naive() == local(b).date_naive()
}

/// Full time for tooltips, as `Intl.DateTimeFormat` (en-US, `dateStyle:
/// "full"`, `timeStyle: "long"`) writes it: "Friday, October 2, 2026 at
/// 3:05:09 PM EDT".
pub fn full(seconds: u64) -> String {
    let time = local(seconds);
    format!(
        "{} {}",
        time.format("%A, %B %-d, %Y at %-I:%M:%S %p"),
        zone(seconds, time.offset().local_minus_utc())
    )
}

/// `dateStyle: "full"`, `timeStyle: "short"`: "Friday, October 2, 2026 at
/// 3:05 PM".
pub fn full_short(seconds: u64) -> String {
    local(seconds)
        .format("%A, %B %-d, %Y at %-I:%M %p")
        .to_string()
}

/// "Oct 2", for dates more than a week old.
pub fn short_month_day(seconds: u64) -> String {
    local(seconds).format("%b %-d").to_string()
}

/// The zone as en-US `Intl` names it: US abbreviations and "UTC" by name,
/// every other zone by its offset ("GMT+2", "GMT+5:30", "GMT").
fn zone(seconds: u64, offset_seconds: i32) -> String {
    const NAMED: &[&str] = &[
        "UTC", "EST", "EDT", "CST", "CDT", "MST", "MDT", "PST", "PDT", "AKST", "AKDT", "HST", "HDT",
    ];
    let abbreviation = local_abbreviation(seconds);
    if let Some(name) = abbreviation.filter(|name| NAMED.contains(&name.as_str())) {
        return name;
    }
    gmt_offset(offset_seconds)
}

fn gmt_offset(offset_seconds: i32) -> String {
    if offset_seconds == 0 {
        return "GMT".into();
    }
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let minutes = offset_seconds.unsigned_abs() / 60;
    match minutes % 60 {
        0 => format!("GMT{sign}{}", minutes / 60),
        rest => format!("GMT{sign}{}:{rest:02}", minutes / 60),
    }
}

/// The C library's abbreviation for the local zone at that instant.
#[cfg(unix)]
fn local_abbreviation(seconds: u64) -> Option<String> {
    let time = seconds as libc::time_t;
    // SAFETY: `localtime_r` fills the zeroed `tm` we own; `tm_zone` then
    // points at static zone data, read before any other zone call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() || tm.tm_zone.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(tm.tm_zone)
            .to_str()
            .ok()
            .map(str::to_string)
    }
}

/// Windows' C library names zones in full ("Pacific Standard Time"), so
/// the caller falls back to a GMT offset there.
#[cfg(not(unix))]
fn local_abbreviation(_seconds: u64) -> Option<String> {
    None
}

/// The longest wait between checks of the local day. Timers run late
/// after a suspend and the time zone can change, so the day is checked at
/// least this often, not only at midnight (`useLocalDay`).
const DAY_CHECK: Duration = Duration::from_secs(15 * 60);

/// Today's local date, which "Today" and "Yesterday" are relative to.
pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// How long until the next local midnight, plus a second so the day has
/// turned, but never longer than [`DAY_CHECK`].
fn until_next_check(now: DateTime<Local>) -> Duration {
    let midnight = now
        .date_naive()
        .succ_opt()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .and_then(|time| Local.from_local_datetime(&time).earliest());
    let until = midnight
        .and_then(|midnight| (midnight - now).to_std().ok())
        .unwrap_or(DAY_CHECK);
    (until + Duration::from_secs(1)).min(DAY_CHECK)
}

/// Calls `changed` whenever the local day turns, so labels such as
/// "Today" and "Yesterday at 9:05 AM" are rebuilt without new rows.
pub fn on_new_day<T: 'static>(
    cx: &mut Context<T>,
    changed: impl Fn(&mut T, &mut Context<T>) + 'static,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut day = today();
        loop {
            cx.background_executor()
                .timer(until_next_check(Local::now()))
                .await;
            let now = today();
            if now == day {
                continue;
            }
            day = now;
            if this.update(cx, |this, cx| changed(this, cx)).is_err() {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).single().unwrap()
    }

    #[test]
    fn zones_read_like_intl() {
        assert_eq!(gmt_offset(0), "GMT");
        assert_eq!(gmt_offset(2 * 3600), "GMT+2");
        assert_eq!(gmt_offset(5 * 3600 + 1800), "GMT+5:30");
        assert_eq!(gmt_offset(-3 * 3600), "GMT-3");
        let full = full(1_700_000_000);
        assert!(full.contains(" at "));
        assert!(full.rsplit(' ').next().is_some_and(|zone| !zone.is_empty()));
    }

    #[test]
    fn bylines_follow_buzz_app() {
        let now = at(2026, 10, 9, 15, 0);
        let s = |t: DateTime<Local>| t.timestamp() as u64;
        assert_eq!(byline_at(s(at(2026, 10, 9, 14, 34)), now), "2:34 PM");
        assert_eq!(
            byline_at(s(at(2026, 10, 8, 9, 5)), now),
            "Yesterday at 9:05 AM"
        );
        assert_eq!(
            byline_at(s(at(2026, 10, 5, 14, 34)), now),
            "Monday at 2:34 PM"
        );
        assert_eq!(
            byline_at(s(at(2026, 6, 20, 14, 34)), now),
            "Sat, Jun 20 at 2:34 PM"
        );
        assert_eq!(
            byline_at(s(at(2025, 6, 20, 14, 34)), now),
            "Jun 20, 2025 at 2:34 PM"
        );
        assert_eq!(day_at(s(at(2026, 6, 20, 1, 0)), now), "Saturday, June 20");
        assert_eq!(day_at(s(at(2025, 6, 20, 1, 0)), now), "June 20, 2025");
    }

    #[test]
    fn the_compact_clock_is_the_byline_clock_without_its_day_period() {
        let s = |t: DateTime<Local>| t.timestamp() as u64;
        for (h, min) in [(9, 5), (12, 0), (0, 30), (14, 34), (23, 59)] {
            let time = at(2026, 10, 9, h, min);
            let byline = byline_at(s(time), time);
            let period = if h < 12 { " AM" } else { " PM" };
            assert_eq!(compact(s(time)), byline.trim_end_matches(period));
        }
        assert_eq!(compact(s(at(2026, 10, 9, 9, 5))), "9:05");
        assert_eq!(compact(s(at(2026, 10, 9, 0, 30))), "12:30");
    }

    #[test]
    fn checks_the_day_just_after_midnight_or_every_quarter_hour() {
        let late = at(2026, 10, 9, 23, 58);
        assert_eq!(until_next_check(late), Duration::from_secs(121));
        assert_eq!(until_next_check(at(2026, 10, 9, 15, 0)), DAY_CHECK);
        // Just after midnight the next check is the regular one.
        assert_eq!(until_next_check(at(2026, 10, 10, 0, 0)), DAY_CHECK);
    }

    #[test]
    fn a_day_label_turns_at_midnight() {
        let s = |t: DateTime<Local>| t.timestamp() as u64;
        let message = s(at(2026, 10, 9, 22, 0));
        assert_eq!(day_at(message, at(2026, 10, 9, 23, 59)), "Today");
        assert_eq!(day_at(message, at(2026, 10, 10, 0, 1)), "Yesterday");
    }
}
