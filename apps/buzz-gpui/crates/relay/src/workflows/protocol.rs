//! The workflow wire protocol (`features/workflows/protocol.ts`,
//! `queries.ts`, `http.ts`): signed coordinates of kind-30620
//! definitions, the 46020 run command and the kind-5 coordinate delete,
//! the structural YAML checks the signer applies, and the relay's run
//! history route.

use super::types::{
    RunCursor, RunPage, RunStatus, WorkflowDefinition, WorkflowReference, WorkflowRun,
};
use crate::event::{Verified, is_hex64};
use serde_json::Value;

pub const DEFINITION: u16 = 30620;
pub const TRIGGER: u16 = 46020;
pub const DELETION: u16 = 5;
/// The relay bounds aggregate explicit channel values at 128.
pub const CHANNEL_BATCH: usize = 128;
pub const DEFINITION_LIMIT: usize = 100;
/// Run history pages, and the size cap on their body.
pub const RUN_PAGE: usize = 20;
pub const READ_BYTES: usize = 1024 * 1024;
/// Workflow YAML larger than this is refused before signing.
pub const MAX_YAML_BYTES: usize = 24_000;

/// Lowercase canonical UUID.
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

pub fn validate_reference(reference: &WorkflowReference) -> Result<(), String> {
    if !is_uuid(&reference.id) || !is_uuid(&reference.channel_id) || !is_hex64(&reference.owner) {
        return Err("Invalid workflow coordinate".into());
    }
    Ok(())
}

/// A command's kind and tags, from a signed event or an outbox intent.
pub struct CommandView<'a> {
    pub kind: u16,
    pub pubkey: &'a str,
    pub content: &'a str,
    pub tags: Vec<&'a [String]>,
}

impl<'a> CommandView<'a> {
    fn one(&self, name: &str) -> Result<&'a str, String> {
        let tags: Vec<&&[String]> = self
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some(name))
            .collect();
        match tags.as_slice() {
            [tag] if tag.len() == 2 && !tag[1].is_empty() => Ok(tag[1].as_str()),
            _ => Err(format!("Invalid workflow {name} tag")),
        }
    }
}

/// Whether a command is a workflow operation (`isWorkflowOperation`). The
/// relay parses the deletion coordinate's kind numerically, `+` and
/// leading zeros included; [`reference`] rejects those aliases.
pub fn is_operation(kind: u16, tags: &[&[String]]) -> bool {
    match kind {
        DEFINITION | TRIGGER => true,
        DELETION => tags.iter().any(|tag| {
            tag.first().map(String::as_str) == Some("a")
                && tag.get(1).is_some_and(|value| {
                    let value = value.strip_prefix('+').unwrap_or(value);
                    value.trim_start_matches('0').starts_with("30620:")
                })
        }),
        _ => false,
    }
}

/// The workflow a command names (`workflowReference`).
pub fn reference(event: &CommandView) -> Result<WorkflowReference, String> {
    let channel_id = event.one("h")?.to_string();
    let (id, owner) = if event.kind == DELETION {
        let parts: Vec<&str> = event.one("a")?.split(':').collect();
        match parts.as_slice() {
            ["30620", owner, id] => (id.to_string(), owner.to_string()),
            _ => return Err("Invalid workflow deletion coordinate".into()),
        }
    } else {
        (event.one("d")?.to_string(), event.pubkey.to_string())
    };
    let reference = WorkflowReference {
        id,
        owner,
        channel_id,
    };
    validate_reference(&reference)?;
    Ok(reference)
}

/// A verified kind-30620 event as a definition (`definition`).
pub fn definition(event: &Verified) -> Result<WorkflowDefinition, String> {
    if event.kind_u16() != DEFINITION {
        return Err("Invalid workflow definition".into());
    }
    let pubkey = event.pubkey_hex();
    let view = CommandView {
        kind: DEFINITION,
        pubkey: &pubkey,
        content: &event.content,
        tags: event.tags.iter().map(|tag| tag.as_slice()).collect(),
    };
    let reference = reference(&view)?;
    Ok(WorkflowDefinition {
        id: reference.id,
        owner: reference.owner,
        channel_id: reference.channel_id,
        revision: event.id_hex(),
        created_at: event.created_at_secs(),
        yaml: event.content.clone(),
    })
}

/// What [`workflow_yaml`] reads from a definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YamlSummary {
    /// Omission means enabled, as the relay's `WorkflowDef` reads it.
    pub enabled: bool,
    pub name: String,
}

/// Structural authoring checks (`workflowYaml`), not a replacement for the
/// relay's language and role validation.
pub fn workflow_yaml(text: &str) -> Result<YamlSummary, String> {
    if text.len() > MAX_YAML_BYTES {
        return Err("Workflow YAML exceeds 24 KB".into());
    }
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|_| "Workflow YAML is invalid".to_string())?;
    let shape = || {
        "Workflow needs a name, boolean enabled state if present, trigger and 1–100 steps"
            .to_string()
    };
    let map = value.as_mapping().ok_or_else(shape)?;
    let get = |key: &str| map.get(serde_yaml::Value::String(key.into()));
    let name = get("name")
        .and_then(serde_yaml::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(shape)?;
    let enabled = match get("enabled") {
        None => true,
        Some(serde_yaml::Value::Bool(enabled)) => *enabled,
        Some(_) => return Err(shape()),
    };
    let trigger = get("trigger")
        .and_then(serde_yaml::Value::as_mapping)
        .ok_or_else(shape)?;
    if !trigger
        .get(serde_yaml::Value::String("on".into()))
        .is_some_and(serde_yaml::Value::is_string)
    {
        return Err(shape());
    }
    let steps = get("steps")
        .and_then(serde_yaml::Value::as_sequence)
        .filter(|steps| !steps.is_empty() && steps.len() <= 100)
        .ok_or_else(shape)?;
    let mut ids = std::collections::HashSet::new();
    for step in steps {
        let step = step.as_mapping();
        let id = step
            .and_then(|step| step.get(serde_yaml::Value::String("id".into())))
            .and_then(serde_yaml::Value::as_str);
        let action = step
            .and_then(|step| step.get(serde_yaml::Value::String("action".into())))
            .is_some_and(serde_yaml::Value::is_string);
        match id {
            Some(id) if step_id(id) && action && ids.insert(id.to_string()) => {}
            _ => return Err("Workflow steps need unique identifiers and actions".into()),
        }
    }
    Ok(YamlSummary {
        enabled,
        name: name.to_string(),
    })
}

/// `^[A-Za-z0-9_]{1,64}$`.
pub fn step_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The signing boundary (`validateWorkflowEvent`): only canonical
/// workflow operations by their author, never a generic kind 5.
pub fn validate_command(event: &CommandView, viewer: &str) -> Result<WorkflowReference, String> {
    if event.tags.len() > 8
        || event
            .tags
            .iter()
            .any(|tag| tag.len() != 2 || tag.iter().any(|value| value.len() > 256))
    {
        return Err("Malformed workflow command".into());
    }
    if !matches!(event.kind, DEFINITION | TRIGGER | DELETION) {
        return Err("Unsupported workflow operation".into());
    }
    let reference = reference(event)?;
    if reference.owner != viewer || event.pubkey != viewer {
        return Err("Only the workflow author can manage it".into());
    }
    let allowed: &[&str] = if event.kind == DELETION {
        &["h", "a", "client-id"]
    } else {
        &["h", "d", "expected-revision", "client-id"]
    };
    let names: std::collections::HashSet<&str> =
        event.tags.iter().map(|tag| tag[0].as_str()).collect();
    if names.len() != event.tags.len() || names.iter().any(|name| !allowed.contains(name)) {
        return Err("Unsupported workflow command tag".into());
    }
    if event.kind != DEFINITION && !event.content.is_empty() {
        return Err("Workflow command content must be empty".into());
    }
    if event.kind == DEFINITION {
        if names.contains("expected-revision") && !is_hex64(event.one("expected-revision")?) {
            return Err("Invalid expected workflow revision".into());
        }
        workflow_yaml(event.content)?;
    } else if names.contains("expected-revision") {
        return Err("Unexpected workflow revision tag".into());
    }
    Ok(reference)
}

/// The relay's inbound webhook route (`hookUrl`); the secret travels in an
/// `X-Webhook-Secret` header.
pub fn hook_url(relay_http: &str, id: &str) -> Result<String, String> {
    if !is_uuid(id) {
        return Err("Invalid workflow ID".into());
    }
    Ok(format!("{relay_http}/hooks/{id}"))
}

/// The fixed run-history route (`runsPath`).
pub fn runs_path(id: &str, cursor: Option<&RunCursor>) -> Result<String, String> {
    if !is_uuid(id) {
        return Err("Invalid workflow ID".into());
    }
    let mut path = format!("/workflows/{id}/runs?limit={RUN_PAGE}");
    if let Some(cursor) = cursor {
        validate_cursor(cursor)?;
        path.push_str(&format!(
            "&before={}&before_id={}",
            url::form_urlencoded::byte_serialize(cursor.before.as_bytes()).collect::<String>(),
            cursor.before_id
        ));
    }
    Ok(path)
}

fn validate_cursor(cursor: &RunCursor) -> Result<(), String> {
    let before = cursor.before.as_bytes();
    let dated = before.len() >= 11
        && before[..10].iter().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                *byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
        && before[10] == b'T';
    let parses = chrono_like(&cursor.before);
    if !is_uuid(&cursor.before_id) || before.len() > 40 || !dated || !parses {
        return Err("Invalid workflow run cursor".into());
    }
    Ok(())
}

/// `Date.parse` accepts the relay's RFC 3339 cursor, fractional seconds
/// and offsets included; check the shape without a date library.
fn chrono_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let number = |range: std::ops::Range<usize>| -> u32 {
        std::str::from_utf8(&bytes[range])
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(u32::MAX)
    };
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let (month, day) = (number(5..7), number(8..10));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
    // Time: HH:MM, optional :SS and fraction, then Z or ±HH:MM.
    let Some(rest) = value.get(11..) else {
        return false;
    };
    let (time, zone) = match rest.find(['Z', '+', '-']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let valid_zone = zone.is_empty()
        || zone == "Z"
        || (zone.len() == 6
            && zone.as_bytes()[3] == b':'
            && zone[1..3].bytes().all(|b| b.is_ascii_digit())
            && zone[4..6].bytes().all(|b| b.is_ascii_digit()));
    let mut parts = time.split(':');
    let hour = parts.next().and_then(|h| h.parse::<u32>().ok());
    let minute = parts.next().and_then(|m| m.parse::<u32>().ok());
    let second = parts.next();
    let second_ok = second.is_none_or(|second| {
        let (whole, fraction) = second.split_once('.').unwrap_or((second, "0"));
        whole.len() == 2
            && whole.parse::<u32>().is_ok_and(|s| s < 60)
            && !fraction.is_empty()
            && fraction.bytes().all(|b| b.is_ascii_digit())
    });
    valid_zone
        && parts.next().is_none()
        && hour.is_some_and(|h| h < 24)
        && minute.is_some_and(|m| m < 60)
        && second_ok
}

fn whole(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value < (1 << 53))
}

fn nullable_whole(value: &Value) -> Result<Option<u64>, ()> {
    if value.is_null() {
        Ok(None)
    } else {
        whole(value).map(Some).ok_or(())
    }
}

fn nullable_text(value: &Value) -> Result<Option<String>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) if text.encode_utf16().count() <= 16_000 => Ok(Some(text.clone())),
        _ => Err(()),
    }
}

/// Validates one page of run history (`parseRuns`).
pub fn parse_runs(raw: &Value, workflow_id: &str) -> Result<RunPage, String> {
    let invalid_row = || "Invalid workflow run row".to_string();
    let runs = raw
        .as_object()
        .and_then(|raw| raw.get("runs"))
        .and_then(Value::as_array)
        .filter(|runs| runs.len() <= RUN_PAGE)
        .ok_or_else(|| "Invalid workflow run response".to_string())?;
    let mut ids = std::collections::HashSet::new();
    let mut parsed = Vec::with_capacity(runs.len());
    for run in runs {
        let run = run.as_object().ok_or_else(invalid_row)?;
        let field = |name: &str| run.get(name).unwrap_or(&Value::Null);
        let id = field("id")
            .as_str()
            .filter(|id| is_uuid(id) && !ids.contains(*id))
            .ok_or_else(invalid_row)?;
        if field("workflow_id").as_str() != Some(workflow_id) {
            return Err(invalid_row());
        }
        let status = field("status")
            .as_str()
            .and_then(RunStatus::parse)
            .ok_or_else(invalid_row)?;
        let current_step = whole(field("current_step")).ok_or_else(invalid_row)?;
        let created_at = whole(field("created_at")).ok_or_else(invalid_row)?;
        let started_at = nullable_whole(field("started_at")).map_err(|_| invalid_row())?;
        let completed_at = nullable_whole(field("completed_at")).map_err(|_| invalid_row())?;
        let trace = field("execution_trace")
            .as_array()
            .filter(|trace| trace.len() <= 1000)
            .ok_or_else(invalid_row)?;
        let error_code = nullable_text(field("error_code")).map_err(|_| invalid_row())?;
        let error_message = nullable_text(field("error_message")).map_err(|_| invalid_row())?;
        ids.insert(id.to_string());
        parsed.push(WorkflowRun {
            id: id.to_string(),
            workflow_id: workflow_id.to_string(),
            status,
            current_step,
            trace: trace.clone(),
            started_at,
            completed_at,
            created_at,
            error_code,
            error_message,
        });
    }
    let next = match raw.get("next") {
        Some(Value::Null) => None,
        Some(next) => {
            let invalid = || "Invalid workflow run cursor".to_string();
            let next = next.as_object().ok_or_else(invalid)?;
            let (Some(before), Some(before_id)) = (
                next.get("before").and_then(Value::as_str),
                next.get("before_id").and_then(Value::as_str),
            ) else {
                return Err(invalid());
            };
            if parsed.is_empty() {
                return Err(invalid());
            }
            let cursor = RunCursor {
                before: before.to_string(),
                before_id: before_id.to_string(),
            };
            validate_cursor(&cursor)?;
            Some(cursor)
        }
        // `raw.next !== null` with `next` absent reads `undefined`.
        None => return Err("Invalid workflow run cursor".into()),
    };
    Ok(RunPage { runs: parsed, next })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use serde_json::json;

    const CHANNEL: &str = "44444444-4444-4444-8444-444444444444";
    const ID: &str = "55555555-5555-4555-8555-555555555555";
    const RUN: &str = "77777777-7777-4777-8777-777777777777";

    fn tags(list: &[[&str; 2]]) -> Vec<Vec<String>> {
        list.iter()
            .map(|[name, value]| vec![name.to_string(), value.to_string()])
            .collect()
    }

    fn view<'a>(
        kind: u16,
        pubkey: &'a str,
        content: &'a str,
        tags: &'a [Vec<String>],
    ) -> CommandView<'a> {
        CommandView {
            kind,
            pubkey,
            content,
            tags: tags.iter().map(Vec::as_slice).collect(),
        }
    }

    const YAML: &str = "name: Helper\ntrigger:\n  on: message_posted\nsteps:\n  - id: a\n    action: send_message\n    text: hi\n";

    fn slices(tags: &[Vec<String>]) -> Vec<&[String]> {
        tags.iter().map(Vec::as_slice).collect()
    }

    #[test]
    fn classifies_operations_like_the_relay() {
        let a = |value: &str| vec![vec!["a".to_string(), value.to_string()]];
        assert!(is_operation(30620, &[]));
        assert!(is_operation(46020, &[]));
        assert!(is_operation(5, &slices(&a("30620:x:y"))));
        assert!(is_operation(5, &slices(&a("+0030620:x:y"))));
        assert!(!is_operation(5, &slices(&a("30621:x:y"))));
        assert!(!is_operation(9, &[]));
    }

    #[test]
    fn references_need_canonical_coordinates() {
        let owner = "11".repeat(32);
        let save = tags(&[["h", CHANNEL], ["d", ID]]);
        let reference = reference(&view(30620, &owner, YAML, &save)).unwrap();
        assert_eq!(reference.id, ID);
        assert_eq!(reference.owner, owner);
        let coordinate = format!("30620:{owner}:{ID}");
        let delete = tags(&[["h", CHANNEL], ["a", &coordinate]]);
        assert_eq!(
            super::reference(&view(5, &owner, "", &delete)).unwrap().id,
            ID
        );
        // An alias the classifier accepts is still refused as a coordinate.
        let alias = format!("+30620:{owner}:{ID}");
        let aliased = tags(&[["h", CHANNEL], ["a", &alias]]);
        assert!(super::reference(&view(5, &owner, "", &aliased)).is_err());
        let upper = tags(&[["h", "ABCDEF00-4444-4444-8444-444444444444"], ["d", ID]]);
        assert!(super::reference(&view(30620, &owner, YAML, &upper)).is_err());
        let twice = tags(&[["h", CHANNEL], ["h", CHANNEL], ["d", ID]]);
        assert_eq!(
            super::reference(&view(30620, &owner, YAML, &twice)),
            Err("Invalid workflow h tag".into())
        );
    }

    #[test]
    fn checks_the_yaml_shape_without_rewriting_it() {
        assert_eq!(
            workflow_yaml(YAML),
            Ok(YamlSummary {
                enabled: true,
                name: "Helper".into()
            })
        );
        let disabled = format!("{YAML}enabled: false\n");
        assert!(!workflow_yaml(&disabled).unwrap().enabled);
        assert!(
            workflow_yaml("name: x\nenabled: maybe\ntrigger: {on: a}\nsteps: [{id: a, action: b}]")
                .is_err()
        );
        assert_eq!(
            workflow_yaml(
                "name: x\ntrigger: {on: a}\nsteps: [{id: a, action: b}, {id: a, action: c}]"
            ),
            Err("Workflow steps need unique identifiers and actions".into())
        );
        assert_eq!(workflow_yaml(": :"), Err("Workflow YAML is invalid".into()));
        assert_eq!(
            workflow_yaml(&"#".repeat(24_001)),
            Err("Workflow YAML exceeds 24 KB".into())
        );
        // `on` stays a string key under the YAML 1.2 core schema.
        assert!(
            workflow_yaml(
                "name: x\ntrigger:\n  on: schedule\nsteps:\n  - id: s\n    action: delay\n"
            )
            .is_ok()
        );
    }

    #[test]
    fn signs_only_canonical_commands_by_their_author() {
        let viewer = "11".repeat(32);
        let other = "22".repeat(32);
        let save = tags(&[["h", CHANNEL], ["d", ID], ["client-id", "x"]]);
        assert!(validate_command(&view(30620, &viewer, YAML, &save), &viewer).is_ok());
        assert_eq!(
            validate_command(&view(30620, &other, YAML, &save), &viewer),
            Err("Only the workflow author can manage it".into())
        );
        let revision = "ab".repeat(32);
        let expected = tags(&[["h", CHANNEL], ["d", ID], ["expected-revision", &revision]]);
        assert!(validate_command(&view(30620, &viewer, YAML, &expected), &viewer).is_ok());
        assert_eq!(
            validate_command(&view(46020, &viewer, "", &expected), &viewer),
            Err("Unexpected workflow revision tag".into())
        );
        let run = tags(&[["h", CHANNEL], ["d", ID]]);
        assert_eq!(
            validate_command(&view(46020, &viewer, "x", &run), &viewer),
            Err("Workflow command content must be empty".into())
        );
        let extra = tags(&[["h", CHANNEL], ["d", ID], ["p", &viewer]]);
        assert_eq!(
            validate_command(&view(46020, &viewer, "", &extra), &viewer),
            Err("Unsupported workflow command tag".into())
        );
        let coordinate = format!("30620:{other}:{ID}");
        let delete = tags(&[["h", CHANNEL], ["a", &coordinate]]);
        assert_eq!(
            validate_command(&view(5, &viewer, "", &delete), &viewer),
            Err("Only the workflow author can manage it".into())
        );
    }

    #[test]
    fn builds_the_fixed_routes() {
        assert_eq!(
            runs_path(ID, None).unwrap(),
            format!("/workflows/{ID}/runs?limit=20")
        );
        let cursor = RunCursor {
            before: "2026-09-12T12:00:00.123456Z".into(),
            before_id: RUN.into(),
        };
        assert_eq!(
            runs_path(ID, Some(&cursor)).unwrap(),
            format!(
                "/workflows/{ID}/runs?limit=20&before=2026-09-12T12%3A00%3A00.123456Z&before_id={RUN}"
            )
        );
        let offset = RunCursor {
            before: "2026-09-12T12:00:00+02:00".into(),
            before_id: RUN.into(),
        };
        assert!(runs_path(ID, Some(&offset)).is_ok());
        let bad = RunCursor {
            before: "yesterday".into(),
            before_id: RUN.into(),
        };
        assert!(runs_path(ID, Some(&bad)).is_err());
        for short in ["2026-09-12", "2026-09-12é12:00Z"] {
            let short = RunCursor {
                before: short.into(),
                before_id: RUN.into(),
            };
            assert!(runs_path(ID, Some(&short)).is_err());
        }
        assert!(runs_path("not-a-uuid", None).is_err());
        assert_eq!(
            hook_url("https://relay.example", ID).unwrap(),
            format!("https://relay.example/hooks/{ID}")
        );
    }

    #[test]
    fn parses_a_run_page() {
        let row = json!({
            "id": RUN, "workflow_id": ID, "status": "waiting_approval",
            "current_step": 1, "execution_trace": [{"step": "a"}],
            "started_at": 1, "completed_at": null, "created_at": 1,
            "error_code": null, "error_message": null,
        });
        let page = parse_runs(
            &json!({"runs": [row], "next": {"before": "2026-09-12T12:00:00.123456Z", "before_id": RUN}}),
            ID,
        )
        .unwrap();
        assert_eq!(page.runs[0].status, RunStatus::WaitingApproval);
        assert_eq!(page.next.unwrap().before_id, RUN);
        assert!(parse_runs(&json!({"runs": [row], "next": null}), "other").is_err());
        assert!(parse_runs(&json!({"runs": [row, row], "next": null}), ID).is_err());
        assert!(
            parse_runs(
                &json!({"runs": [], "next": {"before": "2026-09-12T12:00:00Z", "before_id": RUN}}),
                ID
            )
            .is_err()
        );
        assert!(parse_runs(&json!({"runs": []}), ID).is_err());
        assert_eq!(
            parse_runs(&json!({"runs": [], "next": null}), ID)
                .unwrap()
                .runs
                .len(),
            0
        );
    }
}
