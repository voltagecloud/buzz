//! Compact durations (`workflowDuration.ts`): `5s`, `1h 2s`, `2d`, `3w`.

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;
/// `Number.isSafeInteger`'s bound.
pub const SAFE: u64 = (1 << 53) - 1;

/// Parses a compact duration such as `5s`, `1h 2s`, `2d` or `3w`, or bare
/// whole seconds. Units are case-insensitive and in w, d, h, m, s order.
pub fn parse_seconds(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return trimmed
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= SAFE);
    }
    let units = [
        ('w', WEEK),
        ('d', DAY),
        ('h', HOUR),
        ('m', MINUTE),
        ('s', 1),
    ];
    let mut rest = trimmed;
    let mut next_unit = 0;
    let mut total: u64 = 0;
    let mut any = false;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let number: u64 = rest[..digits].parse().ok()?;
        rest = rest[digits..].trim_start();
        let unit = rest.chars().next()?.to_ascii_lowercase();
        let index = units[next_unit..]
            .iter()
            .position(|(name, _)| *name == unit)?
            + next_unit;
        total = total.checked_add(number.checked_mul(units[index].1)?)?;
        next_unit = index + 1;
        rest = &rest[1..];
        any = true;
    }
    (any && total <= SAFE).then_some(total)
}

fn parts(total: u64) -> [(u64, &'static str, &'static str); 5] {
    [
        (total / WEEK, "w", "week"),
        ((total % WEEK) / DAY, "d", "day"),
        ((total % DAY) / HOUR, "h", "hour"),
        ((total % HOUR) / MINUTE, "m", "minute"),
        (total % MINUTE, "s", "second"),
    ]
}

/// Whole seconds as a compact duration, empty units left out.
pub fn format_seconds(total: u64) -> String {
    if total > SAFE {
        return String::new();
    }
    if total == 0 {
        return "0s".into();
    }
    parts(total)
        .into_iter()
        .filter(|(value, _, _)| *value > 0)
        .map(|(value, unit, _)| format!("{value}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whole seconds with spelled-out, pluralized units, for summaries.
pub fn format_seconds_verbose(total: u64) -> String {
    if total > SAFE {
        return String::new();
    }
    if total == 0 {
        return "0 seconds".into();
    }
    parts(total)
        .into_iter()
        .filter(|(value, _, _)| *value > 0)
        .map(|(value, _, unit)| format!("{value} {unit}{}", if value == 1 { "" } else { "s" }))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn parses_compact_and_combined_durations() {
        assert_eq!(parse_seconds("5s"), Some(5));
        assert_eq!(parse_seconds("90m"), Some(5_400));
        assert_eq!(parse_seconds("1h 2s"), Some(3_602));
        assert_eq!(parse_seconds("1H2M3S"), Some(3_723));
        assert_eq!(parse_seconds("2d"), Some(172_800));
        assert_eq!(parse_seconds("2w 3d 4h 5m 6s"), Some(1_483_506));
        assert_eq!(parse_seconds("42"), Some(42));
        assert_eq!(parse_seconds("0s"), Some(0));
        assert_eq!(parse_seconds(" 1 h "), Some(3_600));
    }

    #[test]
    fn rejects_empty_malformed_and_fractional_values() {
        assert_eq!(parse_seconds(""), None);
        assert_eq!(parse_seconds("1.5m"), None);
        assert_eq!(parse_seconds("1m later"), None);
        assert_eq!(parse_seconds("1s 1m"), None, "units go largest first");
        assert_eq!(parse_seconds("1m 1m"), None, "each unit once");
        assert_eq!(parse_seconds("m"), None);
    }

    #[test]
    fn formats_compact_labels() {
        assert_eq!(format_seconds(0), "0s");
        assert_eq!(format_seconds(5), "5s");
        assert_eq!(format_seconds(62), "1m 2s");
        assert_eq!(format_seconds(3_602), "1h 2s");
        assert_eq!(format_seconds(7_323), "2h 2m 3s");
        assert_eq!(format_seconds(172_800), "2d");
        assert_eq!(format_seconds(1_483_506), "2w 3d 4h 5m 6s");
    }

    #[test]
    fn spells_out_and_pluralizes_units() {
        assert_eq!(format_seconds_verbose(0), "0 seconds");
        assert_eq!(format_seconds_verbose(1), "1 second");
        assert_eq!(format_seconds_verbose(900), "15 minutes");
        assert_eq!(format_seconds_verbose(3_600), "1 hour");
        assert_eq!(format_seconds_verbose(3_662), "1 hour 1 minute 2 seconds");
        assert_eq!(
            format_seconds_verbose(1_483_506),
            "2 weeks 3 days 4 hours 5 minutes 6 seconds"
        );
    }
}
