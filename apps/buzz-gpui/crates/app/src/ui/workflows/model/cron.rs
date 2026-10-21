//! Five-field cron validation (`cronExpression.ts`). The relay's `cron`
//! crate also accepts six and seven fields (seconds and year); Form mode
//! edits the standard five only and leaves the rest to YAML. Weekdays
//! follow the relay: Sunday is 1 through Saturday 7.

pub struct FieldDefinition {
    pub label: &'static str,
    pub min: u32,
    pub max: u32,
    pub aliases: &'static [&'static str],
}

pub const FIELDS: [FieldDefinition; 5] = [
    FieldDefinition {
        label: "Minute",
        min: 0,
        max: 59,
        aliases: &[],
    },
    FieldDefinition {
        label: "Hour",
        min: 0,
        max: 23,
        aliases: &[],
    },
    FieldDefinition {
        label: "Day",
        min: 1,
        max: 31,
        aliases: &[],
    },
    FieldDefinition {
        label: "Month",
        min: 1,
        max: 12,
        aliases: &[
            "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
        ],
    },
    FieldDefinition {
        label: "Weekday",
        min: 1,
        max: 7,
        aliases: &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"],
    },
];

pub type Fields = [String; 5];

/// Splits an expression into the five boxes without inventing fields.
pub fn fields_from_expression(expression: &str) -> Fields {
    let mut values = expression.split_whitespace().map(str::to_owned);
    std::array::from_fn(|_| values.next().unwrap_or_default())
}

pub fn expression_from_fields(fields: &Fields) -> String {
    fields.join(" ")
}

pub fn normalize(expression: &str) -> String {
    expression.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A pasted whole expression, which must have exactly five fields.
pub fn fields_from_paste(pasted: &str) -> Result<Fields, String> {
    let values: Vec<&str> = pasted.split_whitespace().collect();
    // `"".split(/\s+/)` has one empty field.
    let count = values.len().max(1);
    if count != FIELDS.len() {
        return Err(format!(
            "Paste a 5-field cron expression. Found {count} field{}.",
            if count == 1 { "" } else { "s" }
        ));
    }
    Ok(std::array::from_fn(|index| values[index].to_owned()))
}

fn atom_error(atom: &str, definition: &FieldDefinition) -> Option<String> {
    if definition.aliases.contains(&atom.to_uppercase().as_str()) {
        return None;
    }
    if atom.is_empty() || !atom.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(format!(
            "{} contains “{atom}”, which is not a supported value.",
            definition.label
        ));
    }
    let in_range = atom.parse::<u64>().is_ok_and(|value| {
        (u64::from(definition.min)..=u64::from(definition.max)).contains(&value)
    });
    (!in_range).then(|| {
        format!(
            "{} must be between {} and {}.",
            definition.label, definition.min, definition.max
        )
    })
}

fn segment_error(segment: &str, definition: &FieldDefinition) -> Option<String> {
    let steps: Vec<&str> = segment.split('/').collect();
    if steps.len() > 2 || steps.iter().any(|part| part.is_empty()) {
        return Some(format!("{} has an invalid step.", definition.label));
    }
    if let Some(step) = steps.get(1)
        && (!step.bytes().all(|byte| byte.is_ascii_digit()) || step.parse::<u64>().unwrap_or(0) < 1)
    {
        return Some(format!(
            "{} step must be a positive whole number.",
            definition.label
        ));
    }
    let base = steps[0];
    if base == "*" {
        return None;
    }
    let range: Vec<&str> = base.split('-').collect();
    if range.len() > 2 || range.iter().any(|part| part.is_empty()) {
        return Some(format!("{} has an invalid range.", definition.label));
    }
    if let Some(error) = atom_error(range[0], definition) {
        return Some(error);
    }
    let end = range.get(1)?;
    if let Some(error) = atom_error(end, definition) {
        return Some(error);
    }
    // Aliases are not numbers, so their order is not checked.
    match (range[0].parse::<u64>(), end.parse::<u64>()) {
        (Ok(start), Ok(end)) if start > end => Some(format!(
            "{} range must go from lower to higher.",
            definition.label
        )),
        _ => None,
    }
}

pub fn field_error(value: &str, definition: &FieldDefinition) -> Option<String> {
    if value.is_empty() {
        return Some(format!("{} is required.", definition.label));
    }
    let segments: Vec<&str> = value.split(',').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Some(format!("{} has an empty list item.", definition.label));
    }
    segments
        .iter()
        .find_map(|segment| segment_error(segment, definition))
}

pub fn field_errors(fields: &Fields) -> [Option<String>; 5] {
    std::array::from_fn(|index| field_error(&fields[index], &FIELDS[index]))
}

/// The first problem with a whole expression, if any.
pub fn expression_error(expression: &str) -> Option<String> {
    match fields_from_paste(expression) {
        Err(error) => Some(error),
        Ok(fields) => field_errors(&fields).into_iter().flatten().next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn accepts_supported_five_field_syntax() {
        for expression in ["0 9 * * 1-5", "*/15 * * * *", "0 */2 1,15 JAN,MAR MON-FRI"] {
            let fields = fields_from_paste(expression).unwrap();
            assert_eq!(field_errors(&fields), [None, None, None, None, None]);
            assert_eq!(expression_from_fields(&fields), expression);
            assert_eq!(expression_error(expression), None);
        }
    }

    #[test]
    fn validates_ranges_and_structure() {
        let error = |value: &str, index: usize| field_error(value, &FIELDS[index]);
        assert_eq!(error("60", 0).unwrap(), "Minute must be between 0 and 59.");
        assert_eq!(
            error("5-2", 2).unwrap(),
            "Day range must go from lower to higher."
        );
        assert_eq!(
            error("*/0", 1).unwrap(),
            "Hour step must be a positive whole number."
        );
        assert_eq!(error("", 0).unwrap(), "Minute is required.");
        assert_eq!(error("1,,5", 4).unwrap(), "Weekday has an empty list item.");
        assert_eq!(error("1-2-3", 2).unwrap(), "Day has an invalid range.");
        assert_eq!(error("*/5/2", 0).unwrap(), "Minute has an invalid step.");
        assert_eq!(error("mon", 4), None, "aliases are case-insensitive");
        assert_eq!(
            error("MON", 3).unwrap(),
            "Month contains “MON”, which is not a supported value."
        );
    }

    #[test]
    fn needs_exactly_five_fields() {
        assert_eq!(
            fields_from_paste("0 9 * *"),
            Err("Paste a 5-field cron expression. Found 4 fields.".into())
        );
        assert!(
            expression_error("not-a-cron")
                .unwrap()
                .ends_with("Found 1 field.")
        );
        assert!(
            expression_error("0 0 9 * * 1-5")
                .unwrap()
                .ends_with("Found 6 fields.")
        );
        assert_eq!(
            expression_error("0 9 * * 8").unwrap(),
            "Weekday must be between 1 and 7."
        );
    }

    #[test]
    fn splits_without_inventing_fields() {
        assert_eq!(
            fields_from_expression("  0   9 *  * 1-5 "),
            ["0", "9", "*", "*", "1-5"].map(String::from)
        );
        assert_eq!(
            fields_from_expression(""),
            ["", "", "", "", ""].map(String::from)
        );
        assert_eq!(
            fields_from_expression("0 9"),
            ["0", "9", "", "", ""].map(String::from)
        );
        assert_eq!(normalize("  0   9 *  * 1-5 "), "0 9 * * 1-5");
    }

    #[test]
    fn matches_the_relay_weekday_bounds() {
        for weekday in ["0", "0-6", "1,0", "8"] {
            assert_eq!(
                expression_error(&format!("0 9 * * {weekday}")).unwrap(),
                "Weekday must be between 1 and 7."
            );
        }
        for weekday in ["1", "2", "7", "1-7", "SUN", "MON", "SAT"] {
            assert_eq!(expression_error(&format!("0 9 * * {weekday}")), None);
        }
    }
}
