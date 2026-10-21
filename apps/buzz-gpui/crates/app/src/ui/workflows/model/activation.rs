//! Frequency warnings before turning a workflow on
//! (`workflowActivationWarning.ts`): an unfiltered message trigger, or a
//! schedule that runs hourly or more often. Scheduling belongs to the
//! relay; this only reads the shapes it accepts.

use super::duration;
use super::yaml::{self, get};
use serde_yaml::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub title: &'static str,
    pub description: String,
}

const TITLE: &str = "This workflow may run often";
const REVIEW: &str = "Review the schedule before turning it on.";
const HOUR: u64 = 60 * 60;

fn non_empty(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn interval_description(interval: &str) -> Option<String> {
    let seconds = duration::parse_seconds(interval).filter(|s| *s > 0 && *s <= HOUR)?;
    Some(format!(
        "It is scheduled to run every {}. {REVIEW}",
        duration::format_seconds_verbose(seconds)
    ))
}

/// The relay normalizes five- and six-field cron to seven fields.
fn normalized(cron: &str) -> Option<Vec<String>> {
    let fields: Vec<String> = cron.split_whitespace().map(str::to_owned).collect();
    match fields.len() {
        5 => Some(
            std::iter::once("0".to_string())
                .chain(fields)
                .chain(std::iter::once("*".into()))
                .collect(),
        ),
        6 => Some(
            fields
                .into_iter()
                .chain(std::iter::once("*".into()))
                .collect(),
        ),
        7 => Some(fields),
        _ => None,
    }
}

fn all_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// How many times a field fires per cycle of `maximum + 1`, roughly.
fn repeated(field: &str, maximum: u64) -> Option<u64> {
    if field == "*" {
        return Some(maximum + 1);
    }
    if let Some(step) = field.strip_prefix("*/").filter(|s| all_digits(s)) {
        let size: u64 = step.parse().ok()?;
        return (1..=maximum + 1)
            .contains(&size)
            .then(|| (maximum + 1).div_ceil(size));
    }
    if field.contains(',') || field.contains('-') {
        return Some(2);
    }
    all_digits(field).then_some(1)
}

/// Whether an hour field selects every hour (`*`, `*/1`, `0-23` or an
/// equivalent list).
fn every_hour(field: &str) -> bool {
    const HOURS: u64 = 24;
    let mut selected = std::collections::BTreeSet::new();
    for segment in field.split(',') {
        let (base, step) = match segment.split_once('/') {
            Some((base, step)) => (base, Some(step)),
            None => (segment, None),
        };
        if step.is_some_and(|step| !all_digits(step) || step.parse::<u64>().unwrap_or(0) < 1) {
            return false;
        }
        let size = step.map_or(1, |step| step.parse::<u64>().unwrap_or(1));
        let (start, mut end) = if base == "*" {
            (0, HOURS - 1)
        } else {
            let bounds: Vec<&str> = base.split('-').collect();
            if bounds.len() > 2 || bounds.iter().any(|b| !all_digits(b)) {
                return false;
            }
            let start: u64 = bounds[0].parse().unwrap_or(u64::MAX);
            let end: u64 = bounds
                .get(1)
                .map_or(start, |end| end.parse().unwrap_or(u64::MAX));
            if start > end || end >= HOURS {
                return false;
            }
            if bounds.len() == 1 {
                // A bare hour selects only itself; a stepped one runs to
                // the end of the day.
                if step.is_none() {
                    selected.insert(start);
                    continue;
                }
                (start, HOURS - 1)
            } else {
                (start, end)
            }
        };
        if end >= HOURS {
            end = HOURS - 1;
        }
        let mut hour = start;
        while hour <= end {
            selected.insert(hour);
            hour += size;
        }
    }
    selected.len() as u64 == HOURS
}

fn cron_description(cron: &str) -> Option<String> {
    let fields = normalized(cron)?;
    let (second, minute, hour) = (&fields[0], &fields[1], &fields[2]);
    let second_runs = repeated(second, 59)?;
    let minute_runs = repeated(minute, 59)?;
    if !every_hour(hour) {
        return None;
    }
    if second_runs > 1 {
        return Some(format!(
            "It is scheduled to run multiple times a minute. {REVIEW}"
        ));
    }
    if minute == "*" {
        return Some(format!("It is scheduled to run every minute. {REVIEW}"));
    }
    if let Some(step) = minute.strip_prefix("*/").filter(|s| all_digits(s)) {
        let minutes: u64 = step.parse().ok()?;
        return Some(format!(
            "It is scheduled to run every {minutes} minute{}. {REVIEW}",
            if minutes == 1 { "" } else { "s" }
        ));
    }
    match minute_runs {
        1 => Some(format!("It is scheduled to run every hour. {REVIEW}")),
        runs if runs > 1 => Some(format!(
            "It is scheduled to run multiple times an hour. {REVIEW}"
        )),
        _ => None,
    }
}

pub fn warning(text: &str) -> Option<Warning> {
    // Malformed YAML cannot be saved; draft validation reports it.
    let Value::Mapping(definition) = yaml::parse(text).ok()? else {
        return None;
    };
    let trigger = get(&definition, "trigger").and_then(Value::as_mapping)?;
    match non_empty(get(trigger, "on"))? {
        "message_posted" if non_empty(get(trigger, "filter")).is_none() => Some(Warning {
            title: TITLE,
            description: "It will run for every new message in this channel. Review the trigger before turning it on.".into(),
        }),
        "schedule" => {
            let description = match (
                non_empty(get(trigger, "interval")),
                non_empty(get(trigger, "cron")),
            ) {
                (Some(interval), _) => interval_description(interval),
                (None, Some(cron)) => cron_description(cron),
                _ => None,
            }?;
            Some(Warning {
                title: TITLE,
                description,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn schedule(field: &str) -> Option<String> {
        warning(&format!(
            "name: S\ntrigger:\n  on: schedule\n  {field}\nsteps:\n  - id: a\n    action: delay\n    duration: 1m\n"
        ))
        .map(|w| w.description)
    }

    #[test]
    fn warns_about_unfiltered_message_triggers() {
        let unfiltered = warning("name: M\ntrigger:\n  on: message_posted\nsteps: []\n").unwrap();
        assert_eq!(unfiltered.title, "This workflow may run often");
        assert!(
            unfiltered
                .description
                .starts_with("It will run for every new message")
        );
        assert_eq!(
            warning(
                "name: M\ntrigger:\n  on: message_posted\n  filter: 'trigger_text == \"x\"'\nsteps: []\n"
            ),
            None
        );
        assert!(
            warning("name: M\ntrigger:\n  on: message_posted\n  filter: '  '\nsteps: []\n")
                .is_some()
        );
        assert_eq!(
            warning("name: R\ntrigger:\n  on: reaction_added\nsteps: []\n"),
            None
        );
        assert_eq!(warning("not: [valid"), None);
    }

    #[test]
    fn warns_about_frequent_intervals() {
        assert_eq!(
            schedule("interval: 15m").as_deref(),
            Some(
                "It is scheduled to run every 15 minutes. Review the schedule before turning it on."
            )
        );
        assert_eq!(
            schedule("interval: 1h").as_deref(),
            Some("It is scheduled to run every 1 hour. Review the schedule before turning it on.")
        );
        assert_eq!(schedule("interval: 2h"), None);
        assert_eq!(schedule("interval: 0s"), None);
    }

    #[test]
    fn classifies_frequent_cron() {
        let description = |cron: &str| schedule(&format!("cron: '{cron}'"));
        assert!(description("* * * * *").unwrap().contains("every minute"));
        assert!(
            description("*/5 * * * *")
                .unwrap()
                .contains("every 5 minutes")
        );
        assert!(
            description("*/1 * * * *")
                .unwrap()
                .contains("every 1 minute.")
        );
        assert!(description("0 * * * *").unwrap().contains("every hour"));
        assert!(description("0 0-23 * * *").unwrap().contains("every hour"));
        assert!(description("0 */1 * * *").unwrap().contains("every hour"));
        assert!(
            description("0,30 * * * *")
                .unwrap()
                .contains("multiple times an hour")
        );
        assert!(
            description("*/10 * * * * *")
                .unwrap()
                .contains("multiple times a minute")
        );
        assert_eq!(description("0 9 * * *"), None);
        assert_eq!(description("0 */2 * * *"), None);
        assert_eq!(description("0 0-22 * * *"), None);
        assert_eq!(description("0 9 * *"), None);
    }
}
