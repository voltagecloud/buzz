//! Schedule presets (`workflowSchedule.ts`): repeat choices over the cron
//! or interval a schedule trigger carries. Weekday numbers follow the
//! relay, Sunday 1 through Saturday 7; existing numeric fields are never
//! renumbered.

use super::form::{TriggerConfig, TriggerType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frequency {
    Every15Minutes,
    Every30Minutes,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    CustomCron,
    /// A legacy interval that matches no preset: kept editable, never
    /// offered.
    CustomInterval,
}

/// The offered choices, in order.
pub const FREQUENCIES: [Frequency; 7] = [
    Frequency::Every15Minutes,
    Frequency::Every30Minutes,
    Frequency::Hourly,
    Frequency::Daily,
    Frequency::Weekly,
    Frequency::Monthly,
    Frequency::CustomCron,
];

impl Frequency {
    pub fn label(self) -> &'static str {
        match self {
            Self::Every15Minutes => "Every 15 minutes",
            Self::Every30Minutes => "Every 30 minutes",
            Self::Hourly => "Every hour",
            Self::Daily => "Daily",
            Self::Weekly => "Weekly",
            Self::Monthly => "Monthly",
            Self::CustomCron => "Custom cron",
            Self::CustomInterval => "",
        }
    }

    fn interval(self) -> Option<&'static str> {
        match self {
            Self::Every15Minutes => Some("15m"),
            Self::Every30Minutes => Some("30m"),
            Self::Hourly => Some("1h"),
            _ => None,
        }
    }

    pub fn uses_time(self) -> bool {
        matches!(self, Self::Daily | Self::Weekly | Self::Monthly)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleForm {
    pub custom_cron: String,
    pub custom_interval: String,
    pub frequency: Frequency,
    pub month_day: String,
    pub time: String,
    pub weekday: String,
}

const DEFAULT_TIME: &str = "09:00";
/// Monday in the relay's numbering.
const DEFAULT_WEEKDAY: &str = "2";
const DEFAULT_MONTH_DAY: &str = "1";

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Numeric weekday lists and ranges, sorted; named days or a bad range
/// give nothing (they stay custom cron).
pub fn weekdays_from_field(field: &str) -> Vec<String> {
    let mut days = std::collections::BTreeSet::new();
    for segment in field.split(',') {
        if segment.len() == 1 && ('1'..='7').contains(&segment.chars().next().unwrap_or('0')) {
            days.insert(segment.parse::<u8>().unwrap_or(0));
            continue;
        }
        let range = segment.split_once('-').and_then(|(start, end)| {
            let day = |part: &str| {
                (part.len() == 1)
                    .then(|| part.parse::<u8>().ok())
                    .flatten()
                    .filter(|day| (1..=7).contains(day))
            };
            Some((day(start)?, day(end)?))
        });
        match range {
            Some((start, end)) if start <= end => days.extend(start..=end),
            _ => return Vec::new(),
        }
    }
    days.into_iter().map(|day| day.to_string()).collect()
}

struct Common {
    frequency: Frequency,
    month_day: Option<String>,
    time: String,
    weekday: Option<String>,
}

fn parse_common(cron: &str) -> Option<Common> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    let [minute, hour, month_day, month, weekday] = fields.as_slice() else {
        return None;
    };
    if !digits(minute) || !digits(hour) || *month != "*" {
        return None;
    }
    let (minute, hour): (u32, u32) = (minute.parse().ok()?, hour.parse().ok()?);
    if minute > 59 || hour > 23 {
        return None;
    }
    let time = format!("{hour:02}:{minute:02}");
    if *month_day == "*" && *weekday == "*" {
        return Some(Common {
            frequency: Frequency::Daily,
            month_day: None,
            time,
            weekday: None,
        });
    }
    if *month_day == "*" && !weekdays_from_field(weekday).is_empty() {
        return Some(Common {
            frequency: Frequency::Weekly,
            month_day: None,
            time,
            weekday: Some(weekday.to_string()),
        });
    }
    let day_ok = month_day.len() <= 2
        && digits(month_day)
        && !month_day.starts_with('0')
        && month_day
            .parse::<u32>()
            .is_ok_and(|day| (1..=31).contains(&day));
    if *weekday == "*" && day_ok {
        return Some(Common {
            frequency: Frequency::Monthly,
            month_day: Some(month_day.to_string()),
            time,
            weekday: None,
        });
    }
    None
}

/// The repeat choice a schedule trigger shows. A key that is present but
/// empty keeps its custom pane while the user retypes it; only a missing
/// key falls back to Daily.
pub fn form_from_trigger(trigger: &TriggerConfig) -> ScheduleForm {
    if let Some(interval) = &trigger.interval {
        let preset = match interval.trim() {
            "15m" => Some(Frequency::Every15Minutes),
            "30m" => Some(Frequency::Every30Minutes),
            "1h" => Some(Frequency::Hourly),
            _ => None,
        };
        return ScheduleForm {
            custom_cron: String::new(),
            custom_interval: if preset.is_some() {
                String::new()
            } else {
                interval.clone()
            },
            frequency: preset.unwrap_or(Frequency::CustomInterval),
            month_day: DEFAULT_MONTH_DAY.into(),
            time: DEFAULT_TIME.into(),
            weekday: DEFAULT_WEEKDAY.into(),
        };
    }
    let common = trigger
        .cron
        .as_deref()
        .filter(|cron| !cron.is_empty())
        .and_then(parse_common);
    let frequency = match (&common, &trigger.cron) {
        (Some(common), _) => common.frequency,
        (None, Some(_)) => Frequency::CustomCron,
        (None, None) => Frequency::Daily,
    };
    ScheduleForm {
        custom_cron: if common.is_some() {
            String::new()
        } else {
            trigger.cron.clone().unwrap_or_default()
        },
        custom_interval: String::new(),
        frequency,
        month_day: common
            .as_ref()
            .and_then(|c| c.month_day.clone())
            .unwrap_or_else(|| DEFAULT_MONTH_DAY.into()),
        time: common
            .as_ref()
            .map_or_else(|| DEFAULT_TIME.into(), |c| c.time.clone()),
        weekday: common
            .and_then(|c| c.weekday)
            .unwrap_or_else(|| DEFAULT_WEEKDAY.into()),
    }
}

fn cron(cron: String) -> TriggerConfig {
    TriggerConfig {
        cron: Some(cron),
        ..TriggerConfig::new(TriggerType::Schedule)
    }
}

fn interval(interval: String) -> TriggerConfig {
    TriggerConfig {
        interval: Some(interval),
        ..TriggerConfig::new(TriggerType::Schedule)
    }
}

/// `HH:MM` as cron hour and minute, 09:00 when unreadable.
fn cron_time(time: &str) -> (String, String) {
    let parsed = time.split_once(':').and_then(|(hour, minute)| {
        (hour.len() == 2 && minute.len() == 2 && digits(hour) && digits(minute)).then(|| {
            (
                hour.parse::<u32>().unwrap_or(9).to_string(),
                minute.parse::<u32>().unwrap_or(0).to_string(),
            )
        })
    });
    parsed.unwrap_or_else(|| ("9".into(), "0".into()))
}

/// The trigger for a repeat choice: cron or interval, never both (the
/// relay refuses the pair).
pub fn trigger_from_form(form: &ScheduleForm) -> TriggerConfig {
    if let Some(preset) = form.frequency.interval() {
        return interval(preset.into());
    }
    match form.frequency {
        Frequency::CustomInterval => return interval(form.custom_interval.clone()),
        Frequency::CustomCron => return cron(form.custom_cron.clone()),
        _ => {}
    }
    let (hour, minute) = cron_time(&form.time);
    match form.frequency {
        Frequency::Weekly => cron(format!("{minute} {hour} * * {}", form.weekday)),
        Frequency::Monthly => cron(format!("{minute} {hour} {} * *", form.month_day)),
        _ => cron(format!("{minute} {hour} * * *")),
    }
}

/// A new schedule: Daily at 09:00 UTC.
pub fn default_trigger() -> TriggerConfig {
    trigger_from_form(&form_from_trigger(&TriggerConfig::new(
        TriggerType::Schedule,
    )))
}

/// Starts a custom expression from the preset it replaces.
pub fn custom_cron_seed(form: &ScheduleForm) -> String {
    let current = trigger_from_form(form);
    if let Some(cron) = current.cron {
        return cron;
    }
    match current.interval.as_deref() {
        Some("15m") => "*/15 * * * *".into(),
        Some("30m") => "*/30 * * * *".into(),
        Some("1h") => "0 * * * *".into(),
        _ => String::new(),
    }
}

/// "This schedule won’t run in some months." past the 28th.
pub fn month_day_warning(month_day: &str) -> Option<&'static str> {
    (month_day.parse::<u32>().unwrap_or(0) > 28)
        .then_some("This schedule won’t run in some months.")
}

/// The relay's scheduler ticks once a minute and refuses shorter intervals.
pub fn interval_warning(interval: &str) -> Option<&'static str> {
    super::duration::parse_seconds(interval)
        .filter(|seconds| *seconds < 60)
        .map(|_| "The relay requires intervals of at least 60 seconds.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn with_cron(value: &str) -> TriggerConfig {
        cron(value.into())
    }

    fn with_interval(value: &str) -> TriggerConfig {
        interval(value.into())
    }

    #[test]
    fn new_schedules_default_to_daily_at_nine_utc() {
        assert_eq!(default_trigger(), with_cron("0 9 * * *"));
    }

    #[test]
    fn recognizes_interval_presets() {
        let frequency = |value: &str| form_from_trigger(&with_interval(value)).frequency;
        assert_eq!(frequency("15m"), Frequency::Every15Minutes);
        assert_eq!(frequency("30m"), Frequency::Every30Minutes);
        assert_eq!(frequency("1h"), Frequency::Hourly);
        assert_eq!(frequency(" 1h "), Frequency::Hourly);
    }

    #[test]
    fn round_trips_cron_schedules() {
        for value in [
            "30 14 * * *",
            "30 14 * * 1-5",
            "30 14 * * 1,3,5",
            "30 14 * * 4",
            "30 14 23 * *",
            "0 */2 * * 1,3,5",
        ] {
            let form = form_from_trigger(&with_cron(value));
            assert_eq!(trigger_from_form(&form), with_cron(value), "{value}");
        }
    }

    #[test]
    fn parses_common_shapes_into_presets() {
        assert_eq!(
            form_from_trigger(&with_cron("5 7 * * *")),
            ScheduleForm {
                custom_cron: String::new(),
                custom_interval: String::new(),
                frequency: Frequency::Daily,
                month_day: "1".into(),
                time: "07:05".into(),
                weekday: "2".into(),
            }
        );
        let weekly = form_from_trigger(&with_cron("30 14 * * 1-5"));
        assert_eq!(weekly.frequency, Frequency::Weekly);
        assert_eq!(weekly.weekday, "1-5");
        assert_eq!(weekly.time, "14:30");
        let monthly = form_from_trigger(&with_cron("0 0 31 * *"));
        assert_eq!(monthly.frequency, Frequency::Monthly);
        assert_eq!(monthly.month_day, "31");
        assert_eq!(monthly.time, "00:00");
        for value in ["0 9 * * MON", "0 9 * 6 *", "0 24 * * *", "0 9 1 * 1"] {
            assert_eq!(
                form_from_trigger(&with_cron(value)).frequency,
                Frequency::CustomCron,
                "{value}"
            );
        }
    }

    #[test]
    fn keeps_custom_cron_and_legacy_intervals_exactly() {
        let value = " 0 */2 * * 1,3,5 ";
        let form = form_from_trigger(&with_cron(value));
        assert_eq!(form.frequency, Frequency::CustomCron);
        assert_eq!(form.custom_cron, value);
        assert_eq!(trigger_from_form(&form), with_cron(value));
        let legacy = form_from_trigger(&with_interval("2h30m"));
        assert_eq!(legacy.frequency, Frequency::CustomInterval);
        assert_eq!(legacy.custom_interval, "2h30m");
        assert_eq!(trigger_from_form(&legacy), with_interval("2h30m"));
    }

    #[test]
    fn expands_numeric_weekdays() {
        let days = |field: &str| weekdays_from_field(field);
        assert_eq!(days("1-5"), ["1", "2", "3", "4", "5"]);
        assert_eq!(days("1,3,5"), ["1", "3", "5"]);
        assert_eq!(days("5,1,3,1"), ["1", "3", "5"]);
        assert!(days("MON-FRI").is_empty());
        assert!(days("5-1").is_empty());
        assert_eq!(days("7"), ["7"]);
        assert!(days("0").is_empty());
        assert!(days("0-6").is_empty());
    }

    #[test]
    fn never_emits_cron_and_interval_together() {
        let form = form_from_trigger(&with_interval("30m"));
        let custom = trigger_from_form(&ScheduleForm {
            custom_cron: "0 8 * * 6".into(),
            frequency: Frequency::CustomCron,
            ..form
        });
        assert_eq!(custom, with_cron("0 8 * * 6"));
        let preset = trigger_from_form(&ScheduleForm {
            frequency: Frequency::Every15Minutes,
            ..form_from_trigger(&with_cron("0 8 * * 6"))
        });
        assert_eq!(preset, with_interval("15m"));
    }

    #[test]
    fn an_emptied_key_keeps_its_custom_pane() {
        assert_eq!(
            form_from_trigger(&with_interval("")).frequency,
            Frequency::CustomInterval
        );
        assert_eq!(
            form_from_trigger(&with_cron("")).frequency,
            Frequency::CustomCron
        );
        assert_eq!(
            form_from_trigger(&TriggerConfig::new(TriggerType::Schedule)).frequency,
            Frequency::Daily
        );
    }

    #[test]
    fn an_unreadable_time_falls_back_to_nine() {
        let form = form_from_trigger(&with_cron("0 9 * * *"));
        assert_eq!(
            trigger_from_form(&ScheduleForm {
                time: String::new(),
                ..form
            }),
            with_cron("0 9 * * *")
        );
    }

    #[test]
    fn seeds_custom_cron_from_the_preset() {
        let seed = |trigger: TriggerConfig| custom_cron_seed(&form_from_trigger(&trigger));
        assert_eq!(seed(with_interval("15m")), "*/15 * * * *");
        assert_eq!(seed(with_interval("1h")), "0 * * * *");
        assert_eq!(seed(with_cron("30 14 * * 1")), "30 14 * * 1");
        assert_eq!(seed(with_interval("2h")), "");
    }

    #[test]
    fn warns_about_short_months_and_intervals() {
        assert_eq!(month_day_warning("28"), None);
        assert!(month_day_warning("29").is_some());
        assert!(interval_warning("30s").is_some());
        assert_eq!(interval_warning("1m"), None);
        assert_eq!(interval_warning("bad"), None);
    }
}
