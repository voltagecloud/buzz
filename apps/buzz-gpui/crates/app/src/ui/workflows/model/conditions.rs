//! Basic trigger conditions (`workflowConditionExpression.ts`): rows that
//! build evalexpr expressions the relay evaluates, and the reverse parse
//! that keeps anything Basic cannot rebuild losslessly in Advanced.

use super::form::TriggerType;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Equals,
    NotEquals,
    IsNotEmpty,
    IsEmpty,
}

pub const OPERATORS: [Operator; 8] = [
    Operator::Contains,
    Operator::NotContains,
    Operator::StartsWith,
    Operator::EndsWith,
    Operator::Equals,
    Operator::NotEquals,
    Operator::IsNotEmpty,
    Operator::IsEmpty,
];
const EXACT: [Operator; 2] = [Operator::Equals, Operator::NotEquals];

impl Operator {
    pub fn label(self) -> &'static str {
        match self {
            Self::Contains => "Contains",
            Self::NotContains => "Does not contain",
            Self::StartsWith => "Starts with",
            Self::EndsWith => "Ends with",
            Self::Equals => "Equals",
            Self::NotEquals => "Does not equal",
            Self::IsNotEmpty => "Is not empty",
            Self::IsEmpty => "Is empty",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, Self::IsNotEmpty | Self::IsEmpty)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub value: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    pub field: String,
    pub operator: Operator,
    pub value: String,
}

const AUTHOR: Field = Field {
    label: "Author",
    value: "trigger_author",
};

pub fn fields_for(trigger: TriggerType) -> &'static [Field] {
    match trigger {
        TriggerType::MessagePosted => &[
            Field {
                label: "Message text",
                value: "trigger_text",
            },
            AUTHOR,
        ],
        TriggerType::DiffPosted => &[
            Field {
                label: "Diff text",
                value: "trigger_text",
            },
            AUTHOR,
        ],
        TriggerType::ReactionAdded => &[
            Field {
                label: "Reaction emoji",
                value: "trigger_emoji",
            },
            AUTHOR,
            Field {
                label: "Message",
                value: "trigger_message_id",
            },
        ],
        TriggerType::Webhook | TriggerType::Schedule => &[],
    }
}

/// Opaque identifiers compare exactly.
pub fn operators_for(field: &str) -> &'static [Operator] {
    if field == "trigger_author" || field == "trigger_emoji" || field.ends_with("_id") {
        &EXACT
    } else {
        &OPERATORS
    }
}

pub fn default_operator(field: &str) -> Operator {
    operators_for(field)[0]
}

fn hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn identifier(field: &str) -> bool {
    field == "trigger_author" || field.ends_with("_id")
}

pub fn value_error(field: &str, value: &str) -> Option<&'static str> {
    if value.is_empty() {
        return None;
    }
    let trimmed = value.trim();
    if field == "trigger_author" && !hex64(trimmed) {
        return Some("Enter a 64-character hex pubkey.");
    }
    if field.ends_with("_id") && !hex64(trimmed) {
        return Some("Enter a 64-character hex event ID.");
    }
    None
}

/// An incomplete row blocks saving and lossy view switches.
pub fn row_error(row: &Condition) -> Option<&'static str> {
    if !row.operator.needs_value() {
        return None;
    }
    if row.value.is_empty() {
        return Some("Enter a value or remove this condition.");
    }
    value_error(&row.field, &row.value)
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn build(condition: &Condition) -> Option<String> {
    let field = &condition.field;
    match condition.operator {
        Operator::IsNotEmpty => return Some(format!("str_len({field}) > 0")),
        Operator::IsEmpty => return Some(format!("str_len({field}) == 0")),
        _ => {}
    }
    let value = if identifier(field) {
        condition.value.trim()
    } else {
        condition.value.as_str()
    };
    if value.is_empty() || value_error(field, value).is_some() {
        return None;
    }
    let normalized = if identifier(field) {
        value.to_lowercase()
    } else {
        value.to_string()
    };
    let quoted = format!("\"{}\"", escape(&normalized));
    Some(match condition.operator {
        Operator::Contains => format!("str_contains({field}, {quoted})"),
        Operator::NotContains => format!("!str_contains({field}, {quoted})"),
        Operator::StartsWith => format!("str_starts_with({field}, {quoted})"),
        Operator::EndsWith => format!("str_ends_with({field}, {quoted})"),
        Operator::Equals => format!("{field} == {quoted}"),
        Operator::NotEquals => format!("{field} != {quoted}"),
        Operator::IsNotEmpty | Operator::IsEmpty => unreachable!("handled above"),
    })
}

/// Complete rows joined with `&&`; incomplete ones are left out.
pub fn build_all(conditions: &[Condition]) -> String {
    conditions
        .iter()
        .filter_map(build)
        .collect::<Vec<_>>()
        .join(" && ")
}

fn unescape(value: &str) -> String {
    static ESCAPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\\(["\\])"#).unwrap());
    ESCAPED.replace_all(value, "$1").into_owned()
}

fn known(variable: &str, trigger: TriggerType) -> bool {
    fields_for(trigger)
        .iter()
        .any(|field| field.value == variable)
}

const LITERAL: &str = r#""((?:\\["\\]|[^"\\])*)""#;

pub fn parse(expression: &str, trigger: TriggerType) -> Option<Condition> {
    static EMPTY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^str_len\(([A-Za-z_][A-Za-z0-9_]*)\) (>|==) 0$").unwrap());
    static FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"^(!)?str_(contains|starts_with|ends_with)\(([A-Za-z_][A-Za-z0-9_]*), {LITERAL}\)$"
        ))
        .unwrap()
    });
    static EQUALITY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(r"^([A-Za-z_][A-Za-z0-9_]*) (!=|==) {LITERAL}$")).unwrap()
    });
    let trimmed = expression.trim();
    if let Some(captures) = EMPTY.captures(trimmed)
        && known(&captures[1], trigger)
    {
        let operator = if &captures[2] == ">" {
            Operator::IsNotEmpty
        } else {
            Operator::IsEmpty
        };
        if !operators_for(&captures[1]).contains(&operator) {
            return None;
        }
        return Some(Condition {
            field: captures[1].to_string(),
            operator,
            value: String::new(),
        });
    }
    if let Some(captures) = FUNCTION.captures(trimmed)
        && known(&captures[3], trigger)
    {
        let negated = captures.get(1).is_some();
        if negated && &captures[2] != "contains" {
            return None;
        }
        let operator = match (negated, &captures[2]) {
            (true, _) => Operator::NotContains,
            (false, "starts_with") => Operator::StartsWith,
            (false, "ends_with") => Operator::EndsWith,
            _ => Operator::Contains,
        };
        let result = Condition {
            field: captures[3].to_string(),
            operator,
            value: unescape(&captures[4]),
        };
        return (operators_for(&result.field).contains(&result.operator)
            && build(&result).as_deref() == Some(trimmed))
        .then_some(result);
    }
    if let Some(captures) = EQUALITY.captures(trimmed)
        && known(&captures[1], trigger)
    {
        let result = Condition {
            field: captures[1].to_string(),
            operator: if &captures[2] == "==" {
                Operator::Equals
            } else {
                Operator::NotEquals
            },
            value: unescape(&captures[3]),
        };
        // Noncanonical identifiers and empty literals are not silently
        // normalized or dropped when another row is edited.
        return (build(&result).as_deref() == Some(trimmed)).then_some(result);
    }
    None
}

/// Splits on top-level `&&`, outside strings and parentheses.
fn split_conjunctions(expression: &str) -> Option<Vec<String>> {
    let bytes = expression.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut depth, mut in_string, mut escaped) = (0usize, 0i32, false, false);
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            b'&' if bytes.get(index + 1) == Some(&b'&') && depth == 0 => {
                let part = expression[start..index].trim();
                if part.is_empty() {
                    return None;
                }
                parts.push(part.to_string());
                index += 1;
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    if in_string || depth != 0 {
        return None;
    }
    let last = expression[start..].trim();
    if last.is_empty() {
        return None;
    }
    parts.push(last.to_string());
    Some(parts)
}

/// Basic rows for an expression, or `None` when only Advanced can show it.
pub fn parse_all(expression: &str, trigger: TriggerType) -> Option<Vec<Condition>> {
    if expression.trim().is_empty() {
        return Some(Vec::new());
    }
    let conditions = split_conjunctions(expression)?
        .iter()
        .map(|part| parse(part, trigger))
        .collect::<Option<Vec<_>>>()?;
    let fields: std::collections::HashSet<&str> =
        conditions.iter().map(|c| c.field.as_str()).collect();
    (fields.len() == conditions.len()).then_some(conditions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn row(field: &str, operator: Operator, value: &str) -> Condition {
        Condition {
            field: field.into(),
            operator,
            value: value.into(),
        }
    }

    fn author() -> String {
        "a".repeat(64)
    }

    fn message() -> String {
        "b".repeat(64)
    }

    #[test]
    fn builds_and_escapes_text_conditions() {
        assert_eq!(
            build(&row(
                "trigger_text",
                Operator::Contains,
                "deploy \"buzz\"\\path"
            ))
            .unwrap(),
            r#"str_contains(trigger_text, "deploy \"buzz\"\\path")"#
        );
    }

    #[test]
    fn limits_identifiers_to_equality_and_validates_hex() {
        for field in [
            "trigger_author",
            "trigger_emoji",
            "trigger_message_id",
            "future_id",
        ] {
            assert_eq!(operators_for(field), EXACT);
        }
        assert_eq!(
            value_error("trigger_author", "abc"),
            Some("Enter a 64-character hex pubkey.")
        );
        assert_eq!(value_error("trigger_author", &author()), None);
        assert_eq!(
            value_error("trigger_message_id", &"z".repeat(64)),
            Some("Enter a 64-character hex event ID.")
        );
        assert_eq!(value_error("trigger_message_id", &message()), None);
    }

    #[test]
    fn lowercases_generated_identifiers() {
        assert_eq!(
            build_all(&[
                row("trigger_author", Operator::Equals, &"A".repeat(64)),
                row("trigger_message_id", Operator::NotEquals, &"B".repeat(64)),
            ]),
            format!(
                "trigger_author == \"{}\" && trigger_message_id != \"{}\"",
                author(),
                message()
            )
        );
    }

    #[test]
    fn exposes_only_trigger_fields() {
        let values =
            |trigger| -> Vec<&str> { fields_for(trigger).iter().map(|f| f.value).collect() };
        assert_eq!(
            values(TriggerType::ReactionAdded),
            ["trigger_emoji", "trigger_author", "trigger_message_id"]
        );
        assert_eq!(
            values(TriggerType::MessagePosted),
            ["trigger_text", "trigger_author"]
        );
        assert!(values(TriggerType::Schedule).is_empty());
    }

    #[test]
    fn round_trips_reaction_conjunctions() {
        let conditions = vec![
            row("trigger_emoji", Operator::Equals, "👾"),
            row("trigger_author", Operator::NotEquals, &author()),
            row("trigger_message_id", Operator::Equals, &message()),
        ];
        let expression = build_all(&conditions);
        assert_eq!(
            expression,
            format!(
                "trigger_emoji == \"👾\" && trigger_author != \"{}\" && trigger_message_id == \"{}\"",
                author(),
                message()
            )
        );
        assert_eq!(
            parse_all(&expression, TriggerType::ReactionAdded),
            Some(conditions)
        );
    }

    #[test]
    fn keeps_conjunctions_inside_values_and_rejects_duplicates() {
        let expression = format!(
            "str_contains(trigger_text, \"one && two\") && trigger_author == \"{}\"",
            author()
        );
        assert_eq!(
            parse_all(&expression, TriggerType::MessagePosted).map(|rows| rows.len()),
            Some(2)
        );
        assert_eq!(
            parse_all(
                &format!(
                    "trigger_author == \"{}\" && trigger_author != \"{}\"",
                    author(),
                    message()
                ),
                TriggerType::MessagePosted
            ),
            None
        );
    }

    #[test]
    fn keeps_unsupported_expressions_advanced() {
        let message = TriggerType::MessagePosted;
        assert_eq!(parse("trigger_timestamp > 0", message), None);
        assert_eq!(parse("trigger_emoji == \"👍\"", message), None);
        assert_eq!(parse("trigger_author == \"abc\"", message), None);
        assert_eq!(parse("str_len(trigger_author) == 0", message), None);
        assert_eq!(
            parse(
                "str_len(trigger_message_id) > 0",
                TriggerType::ReactionAdded
            ),
            None
        );
    }

    #[test]
    fn preserves_whitespace_in_text_literals() {
        for (trigger, expression) in [
            (
                TriggerType::MessagePosted,
                r#"str_starts_with(trigger_text, "deploy ")"#,
            ),
            (
                TriggerType::MessagePosted,
                r#"str_ends_with(trigger_text, " deploy")"#,
            ),
            (
                TriggerType::MessagePosted,
                r#"str_contains(trigger_text, " ")"#,
            ),
            (TriggerType::MessagePosted, r#"trigger_text == " deploy ""#),
            (TriggerType::ReactionAdded, r#"trigger_emoji == " 👾 ""#),
        ] {
            let mut rows = parse_all(expression, trigger).unwrap();
            assert_eq!(row_error(&rows[0]), None);
            assert_eq!(build_all(&rows), expression);
            rows.push(row("trigger_author", Operator::Equals, &author()));
            assert_eq!(
                build_all(&rows),
                format!("{expression} && trigger_author == \"{}\"", author())
            );
        }
    }

    #[test]
    fn keeps_literals_basic_would_normalize_in_advanced() {
        for expression in [
            format!("trigger_author == \" {} \"", author()),
            format!("trigger_author == \"{}\"", author().to_uppercase()),
            "trigger_text == \"\"".to_string(),
            "str_contains(trigger_text, \"\")".to_string(),
        ] {
            assert_eq!(parse_all(&expression, TriggerType::MessagePosted), None);
        }
        assert_eq!(
            row_error(&row("trigger_author", Operator::Equals, "   ")),
            Some("Enter a 64-character hex pubkey.")
        );
    }
}
