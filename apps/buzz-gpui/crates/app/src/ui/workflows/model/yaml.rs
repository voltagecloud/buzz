//! YAML for workflow definitions: an emitter that writes the block style
//! buzz-app's `yaml` stringify produces for form state, and the header
//! fields (`workflowYamlDocument.ts`) read and edited without requiring
//! the whole definition to be valid.
//!
//! Header edits change only the `name` or `enabled` entry and keep every
//! other line, comments included, as written. A document the line edit
//! cannot reproduce exactly is written again from its parsed value.

use serde_yaml::{Mapping, Value};
use std::fmt::Write;

/// Parses one YAML document. Duplicate keys and syntax errors fail.
pub fn parse(text: &str) -> Result<Value, String> {
    serde_yaml::from_str::<Value>(text).map_err(|error| error.to_string())
}

/// A key as JavaScript's `Object.keys` names it.
pub fn key_text(key: &Value) -> String {
    match key {
        Value::String(text) => text.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => "null".into(),
        _ => String::new(),
    }
}

pub fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::String(key.into()))
}

// ---------------------------------------------------------------------------
// Emitting

/// Whether a plain scalar would read back as something other than this
/// string under the YAML 1.2 core schema.
fn resolves_to_other(value: &str) -> bool {
    let core = matches!(
        value,
        "" | "~"
            | "null"
            | "Null"
            | "NULL"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
    ) || matches!(
        value,
        ".inf"
            | ".Inf"
            | ".INF"
            | "+.inf"
            | "+.Inf"
            | "+.INF"
            | "-.inf"
            | "-.Inf"
            | "-.INF"
            | ".nan"
            | ".NaN"
            | ".NAN"
    );
    if core {
        return true;
    }
    let body = value.strip_prefix(['-', '+']).unwrap_or(value);
    let int = !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit());
    let octal = value
        .strip_prefix("0o")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| (b'0'..=b'7').contains(&b)));
    let hex = value
        .strip_prefix("0x")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()));
    int || octal || hex || float(body)
}

/// `(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`
fn float(body: &str) -> bool {
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(at) => (&body[..at], Some(&body[at + 1..])),
        None => (body, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            whole.bytes().all(|b| b.is_ascii_digit())
                && fraction.bytes().all(|b| b.is_ascii_digit())
                && !(whole.is_empty() && fraction.is_empty())
        }
        None => !mantissa.is_empty() && mantissa.bytes().all(|b| b.is_ascii_digit()),
    };
    let exponent_ok = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    });
    mantissa_ok && exponent_ok
}

/// The `yaml` package's test for a string that cannot be written plain.
fn needs_quotes(value: &str) -> bool {
    let first = value.chars().next();
    let last = value.chars().last();
    let starts_badly = first.is_some_and(|c| "\n\t ,[]{}#&*!|>'\"%@`".contains(c))
        || value == "?"
        || value == "-"
        || value.starts_with("? ")
        || value.starts_with("?\t")
        || value.starts_with("- ")
        || value.starts_with("-\t");
    let inner = value.contains(": ")
        || value.contains(":\t")
        || value.contains("\n ")
        || value.contains("\n\t")
        || value.contains(" \n")
        || value.contains("\t\n")
        || value.contains(" #")
        || value.contains("\t#")
        || value.contains("\n#");
    let ends_badly = last.is_some_and(|c| "\n\t :".contains(c));
    let marker = value.starts_with("---") || value.starts_with("...");
    starts_badly || inner || ends_badly || marker || resolves_to_other(value)
}

fn double_quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn quoted(value: &str) -> String {
    // Double quotes, unless the value has double quotes and no single ones.
    if value.contains('"') && !value.contains('\'') && !value.contains('\n') {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        double_quoted(value)
    }
}

/// A string scalar after `key: `; `indent` is the key's column.
fn string(value: &str, indent: usize, key: bool) -> String {
    if value.contains('\n') && !key {
        return block(value, indent);
    }
    if needs_quotes(value) {
        quoted(value)
    } else {
        value.to_string()
    }
}

/// A literal block scalar, `|` with its chomping and, when the text
/// starts with whitespace, an indentation indicator.
fn block(value: &str, indent: usize) -> String {
    let trailing = value.len() - value.trim_end_matches('\n').len();
    let chomp = match trailing {
        0 => "-",
        1 => "",
        _ => "+",
    };
    let indicator = if value.starts_with([' ', '\t', '\n']) {
        "2"
    } else {
        ""
    };
    let body = value.strip_suffix('\n').unwrap_or(value);
    let pad = " ".repeat(indent + 2);
    let mut out = format!("|{indicator}{chomp}");
    for line in body.split('\n') {
        out.push('\n');
        if !line.is_empty() {
            out.push_str(&pad);
            out.push_str(line);
        }
    }
    out
}

fn scalar(value: &Value, indent: usize) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => string(text, indent, false),
        Value::Tagged(tagged) => format!("{} {}", tagged.tag, scalar(&tagged.value, indent)),
        // Only a tagged collection at the top of a document gets here.
        Value::Sequence(_) | Value::Mapping(_) => flow(value),
    }
}

/// A value on one line in flow style, for where block style cannot go.
fn flow(value: &Value) -> String {
    match value {
        Value::String(text)
            if !needs_quotes(text) && !text.contains([',', '[', ']', '{', '}', '\n']) =>
        {
            text.clone()
        }
        Value::String(text) => double_quoted(text),
        Value::Tagged(tagged) => format!("{} {}", tagged.tag, flow(&tagged.value)),
        Value::Sequence(items) => {
            let items: Vec<String> = items.iter().map(flow).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Mapping(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(name, value)| format!("{}: {}", flow(name), flow(value)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        other => scalar(other, 0),
    }
}

fn key(value: &Value) -> String {
    match value {
        Value::String(text) => string(text, 0, true),
        Value::Null | Value::Bool(_) | Value::Number(_) => scalar(value, 0),
        Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => flow(value),
    }
}

fn mapping(map: &Mapping, indent: usize, out: &mut String, first_inline: bool) {
    for (index, (name, value)) in map.iter().enumerate() {
        if !(first_inline && index == 0) {
            out.push_str(&" ".repeat(indent));
        }
        out.push_str(&key(name));
        out.push(':');
        entry(value, indent, out);
    }
}

/// What follows `key:` on its line and below.
fn entry(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::Tagged(tagged) => {
            out.push(' ');
            out.push_str(&tagged.tag.to_string());
            entry(&tagged.value, indent, out);
        }
        Value::Mapping(map) if map.is_empty() => out.push_str(" {}\n"),
        Value::Mapping(map) => {
            out.push('\n');
            mapping(map, indent + 2, out, false);
        }
        Value::Sequence(items) if items.is_empty() => out.push_str(" []\n"),
        Value::Sequence(items) => {
            out.push('\n');
            sequence(items, indent + 2, out);
        }
        other => {
            out.push(' ');
            out.push_str(&scalar(other, indent));
            out.push('\n');
        }
    }
}

fn sequence(items: &[Value], indent: usize, out: &mut String) {
    for item in items {
        out.push_str(&" ".repeat(indent));
        out.push('-');
        match item {
            Value::Mapping(map) if !map.is_empty() => {
                out.push(' ');
                mapping(map, indent + 2, out, true);
            }
            Value::Sequence(inner) if !inner.is_empty() => {
                out.push('\n');
                sequence(inner, indent + 2, out);
            }
            other => entry(other, indent, out),
        }
    }
}

/// Writes a value in block style, as `yaml`'s `stringify` does.
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::Mapping(map) if !map.is_empty() => mapping(map, 0, &mut out, false),
        Value::Sequence(items) if !items.is_empty() => sequence(items, 0, &mut out),
        Value::Mapping(_) => out.push_str("{}\n"),
        Value::Sequence(_) => out.push_str("[]\n"),
        other => {
            out.push_str(&scalar(other, 0));
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Header fields

/// The header of a definition: what the dialog header and the card switch
/// need, readable while the rest is still incomplete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentFields {
    /// Top-level keys can be written back into the document.
    pub editable: bool,
    /// The explicit `enabled` value; `None` when absent.
    pub enabled: Option<bool>,
    /// The top-level `name` string; `None` when absent or not a string.
    pub name: Option<String>,
}

impl DocumentFields {
    /// Omission means enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
    }
}

pub fn read_fields(text: &str) -> DocumentFields {
    if text.trim().is_empty() {
        return DocumentFields {
            editable: true,
            enabled: None,
            name: None,
        };
    }
    match parse(text) {
        Ok(Value::Mapping(map)) => DocumentFields {
            editable: true,
            enabled: get(&map, "enabled").and_then(Value::as_bool),
            name: get(&map, "name").and_then(Value::as_str).map(str::to_owned),
        },
        _ => DocumentFields {
            editable: false,
            enabled: None,
            name: None,
        },
    }
}

/// Whether a line starts a new top-level node.
fn top_level(line: &str) -> bool {
    !line.is_empty() && !line.starts_with([' ', '\t'])
}

fn names_key(line: &str, name: &str) -> bool {
    [name.to_string(), format!("\"{name}\""), format!("'{name}'")]
        .iter()
        .any(|quoted| {
            line.strip_prefix(quoted.as_str())
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        })
}

/// Sets (`Some`) or removes (`None`) a top-level entry, editing only its
/// lines. `None` when the document is not a mapping.
fn with_entry(text: &str, name: &str, value: Option<Value>) -> Option<String> {
    let Value::Mapping(mut expected) = parse(text).ok()? else {
        return None;
    };
    match &value {
        Some(value) => {
            expected.insert(Value::String(name.into()), value.clone());
        }
        None => {
            expected.remove(Value::String(name.into()));
        }
    }
    let line = value.as_ref().map(|value| {
        let mut out = name.to_string();
        out.push(':');
        entry(value, 0, &mut out);
        out
    });
    let edited = edit_lines(text, name, line.as_deref());
    let faithful = edited
        .as_deref()
        .and_then(|edited| parse(edited).ok())
        .is_some_and(|parsed| parsed == Value::Mapping(expected.clone()));
    Some(match edited {
        Some(edited) if faithful => edited,
        _ => stringify(&Value::Mapping(expected)),
    })
}

/// Replaces or removes `name`'s lines, or appends `line` at the end.
fn edit_lines(text: &str, name: &str, line: Option<&str>) -> Option<String> {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))?;
    // Flow mappings and documents that start indented are rewritten.
    if !top_level(first) || first.starts_with('{') {
        return None;
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = lines.iter().position(|l| names_key(l, name));
    let mut out = String::new();
    match start {
        Some(start) => {
            // The entry runs until the next top-level line; trailing blank
            // lines stay with what follows.
            let mut end = start + 1;
            while end < lines.len() && !top_level(lines[end].trim_end_matches(['\n', '\r'])) {
                end += 1;
            }
            while end > start + 1 && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            for l in &lines[..start] {
                out.push_str(l);
            }
            if let Some(line) = line {
                out.push_str(line);
            }
            for l in &lines[end..] {
                out.push_str(l);
            }
        }
        None => {
            let line = line?;
            out.push_str(text);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    Some(out)
}

/// Writes `name`; a blank document becomes a fresh definition.
pub fn with_name(text: &str, name: &str) -> Option<String> {
    if text.trim().is_empty() {
        return Some(super::form::form_to_yaml(&super::form::WorkflowForm {
            name: name.into(),
            ..Default::default()
        }));
    }
    with_entry(text, "name", Some(Value::String(name.into())))
}

/// Turning on removes `enabled` (omission means enabled); turning off
/// writes `enabled: false`.
pub fn with_enabled(text: &str, enabled: bool) -> Option<String> {
    if text.trim().is_empty() {
        return Some(super::form::form_to_yaml(&super::form::WorkflowForm {
            enabled,
            ..Default::default()
        }));
    }
    with_entry(text, "enabled", (!enabled).then_some(Value::Bool(false)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    const FIXTURE: &str = "# Keep this comment on opening\nname: Message helper\nenabled: false\ntrigger:\n  on: message_posted\nsteps:\n  - id: notify\n    action: send_message\n    text: Hello from a fixture\n";

    #[test]
    fn reads_header_fields_from_incomplete_definitions() {
        assert_eq!(
            read_fields("name: Draft\nsteps:\n  - id: a\n"),
            DocumentFields {
                editable: true,
                enabled: None,
                name: Some("Draft".into())
            }
        );
        assert_eq!(read_fields(FIXTURE).enabled, Some(false));
        assert_eq!(
            read_fields(""),
            DocumentFields {
                editable: true,
                enabled: None,
                name: None
            }
        );
        assert!(!read_fields("- a\n- b\n").editable);
        assert!(!read_fields("name: [unclosed").editable);
        assert_eq!(read_fields("name: 42\n").name, None);
    }

    #[test]
    fn edits_only_the_header_line_and_keeps_comments() {
        let renamed = with_name(FIXTURE, "Renamed").unwrap();
        assert_eq!(renamed, FIXTURE.replace("Message helper", "Renamed"));
        let enabled = with_enabled(FIXTURE, true).unwrap();
        assert_eq!(enabled, FIXTURE.replace("enabled: false\n", ""));
        let disabled = with_enabled(&enabled, false).unwrap();
        assert!(disabled.starts_with("# Keep this comment on opening\n"));
        assert!(disabled.ends_with("enabled: false\n"));
        assert_eq!(read_fields(&disabled).enabled, Some(false));
        // Quoting follows what the value needs.
        let odd = with_name(FIXTURE, "a: b").unwrap();
        assert!(odd.contains("name: \"a: b\"\n"));
        assert_eq!(read_fields(&odd).name.as_deref(), Some("a: b"));
    }

    #[test]
    fn rewrites_documents_the_line_edit_cannot_keep() {
        let flow = "{name: x, trigger: {on: webhook}, steps: []}";
        let renamed = with_name(flow, "y").unwrap();
        assert_eq!(read_fields(&renamed).name.as_deref(), Some("y"));
        assert!(renamed.starts_with("name: y\n"));
        // A multi-line name is replaced whole.
        let block = "name: |\n  first\n  second\nsteps: []\n";
        assert_eq!(with_name(block, "z").unwrap(), "name: z\nsteps: []\n");
        assert_eq!(with_name("- a\n", "x"), None);
        assert_eq!(with_enabled("name: [", true), None);
    }

    #[test]
    fn a_blank_document_becomes_a_fresh_definition() {
        assert_eq!(
            with_name("", "New").unwrap(),
            "name: New\ntrigger:\n  on: message_posted\nsteps: []\nenabled: false\n"
        );
    }

    #[test]
    fn writes_block_style_like_the_yaml_package() {
        let value = parse(
            "name: x\ntrigger:\n  on: message_posted\nsteps:\n  - id: a\n    action: send_message\n    text: \"\"\n    headers:\n      X-A: b\n",
        )
        .unwrap();
        assert_eq!(
            stringify(&value),
            "name: x\ntrigger:\n  on: message_posted\nsteps:\n  - id: a\n    action: send_message\n    text: \"\"\n    headers:\n      X-A: b\n"
        );
    }

    #[test]
    fn quotes_only_what_would_not_read_back() {
        for (value, written) in [
            ("hello world", "hello world"),
            ("true", "\"true\""),
            ("42", "\"42\""),
            ("1.5", "\"1.5\""),
            ("-1e3", "\"-1e3\""),
            ("null", "\"null\""),
            ("", "\"\""),
            ("a: b", "\"a: b\""),
            ("# x", "\"# x\""),
            (" lead", "\" lead\""),
            ("say \"hi\"", "say \"hi\""),
            ("\"hi\" there", "'\"hi\" there'"),
            ("it's", "it's"),
            ("“Hello”", "“Hello”"),
            (
                "str_contains(trigger_text, \"x\")",
                "str_contains(trigger_text, \"x\")",
            ),
        ] {
            assert_eq!(string(value, 0, false), written, "{value}");
            let line = format!("k: {written}\n");
            let Value::Mapping(map) = parse(&line).unwrap() else {
                panic!()
            };
            assert_eq!(get(&map, "k").and_then(Value::as_str), Some(value));
        }
    }

    #[test]
    fn writes_multi_line_text_as_literal_blocks() {
        for text in [
            "one\ntwo",
            "one\ntwo\n",
            "one\n\ntwo\n\n",
            " indented\nnext",
        ] {
            let mut map = Mapping::new();
            map.insert("text".into(), Value::String(text.into()));
            let mut steps = Mapping::new();
            steps.insert("steps".into(), Value::Sequence(vec![Value::Mapping(map)]));
            let written = stringify(&Value::Mapping(steps));
            let parsed = parse(&written).unwrap();
            assert_eq!(parsed["steps"][0]["text"].as_str(), Some(text), "{written}");
        }
        assert_eq!(block("a\nb", 0), "|-\n  a\n  b");
    }

    #[test]
    fn writes_tagged_collections_and_collection_keys() {
        for text in [
            "{name: a, x: !t {b: 1}}",
            "{name: a, x: !t [1, {c: !u d}], y: [!t {b: 1}, !t [2]], z: !t {}}",
            "{name: a, [k, 1]: v, {m: \"n, o\"}: w, ? !t {p: q} : r}",
        ] {
            let renamed = with_name(text, "b").unwrap();
            let toggled = with_enabled(&renamed, false).unwrap();
            let Value::Mapping(mut expected) = parse(text).unwrap() else {
                panic!()
            };
            expected.insert("name".into(), "b".into());
            expected.insert("enabled".into(), false.into());
            assert_eq!(
                parse(&toggled).unwrap(),
                Value::Mapping(expected),
                "{toggled}"
            );
        }
        let tagged = parse("!t {b: [1, \"x, y\"]}").unwrap();
        assert_eq!(parse(&stringify(&tagged)).unwrap(), tagged);
    }
}
