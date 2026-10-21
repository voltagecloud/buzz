//! The attention tools (buzz-app's `src/buzz-mcp/attention.ts`): an agent
//! manages its own Interests, event watches and timers, with Janet's verbs
//! and scoped object contract. Only the in-app server offers them, and only
//! while the owner has attention on for the agent. Every answer is JSON and
//! names whether a classifier can run now, so the agent never configures
//! one that it believes works when it does not.

use super::tools::tool;
use crate::agents2::objects::{
    ClassifierAvailability, Shown, WatchFilter, WatchStatus, WatchSummary, WriteOptions,
};
use crate::agents2::runtime::AgentAttention;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

type Args = Map<String, Value>;

/// What the attention tools need of the agent's attention: buzz-app's
/// `AgentAttention`.
pub trait Attention: Send + Sync {
    fn enabled(&self) -> BoxFuture<'static, Result<bool, String>>;
    fn classifier(&self) -> ClassifierAvailability;
    fn show(
        &self,
        slug: String,
        interest: Option<String>,
    ) -> BoxFuture<'static, Result<Shown, String>>;
    fn interests(&self, search: String) -> BoxFuture<'static, Result<Vec<String>, String>>;
    fn watches(&self, filter: WatchFilter)
    -> BoxFuture<'static, Result<Vec<WatchSummary>, String>>;
    fn write(
        &self,
        slug: String,
        value: Option<Value>,
        options: WriteOptions,
    ) -> BoxFuture<'static, Result<Shown, String>>;
    /// Unix seconds, for `since: "now"` and a rearm's time.
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs())
    }
}

impl Attention for AgentAttention {
    fn enabled(&self) -> BoxFuture<'static, Result<bool, String>> {
        let this = self.clone();
        async move { this.enabled().await }.boxed()
    }
    fn classifier(&self) -> ClassifierAvailability {
        AgentAttention::classifier(self)
    }
    fn show(
        &self,
        slug: String,
        interest: Option<String>,
    ) -> BoxFuture<'static, Result<Shown, String>> {
        let this = self.clone();
        async move { this.show(&slug, interest.as_deref()).await }.boxed()
    }
    fn interests(&self, search: String) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let this = self.clone();
        async move { this.interests(&search).await }.boxed()
    }
    fn watches(
        &self,
        filter: WatchFilter,
    ) -> BoxFuture<'static, Result<Vec<WatchSummary>, String>> {
        let this = self.clone();
        async move { this.watches(filter).await }.boxed()
    }
    fn write(
        &self,
        slug: String,
        value: Option<Value>,
        options: WriteOptions,
    ) -> BoxFuture<'static, Result<Shown, String>> {
        let this = self.clone();
        async move { this.write(&slug, value, options).await }.boxed()
    }
}

const NAMES: [&str; 12] = [
    "interest_set",
    "interest_show",
    "interest_list",
    "interest_remove",
    "watch_add",
    "watch_update",
    "watch_enable",
    "watch_disable",
    "watch_remove",
    "watch_rearm",
    "watch_show",
    "watch_list",
];

pub fn is_attention_tool(name: &str) -> bool {
    NAMES.contains(&name)
}

/// The attention tools as `tools/list` describes them.
pub fn attention_tools() -> Vec<Value> {
    let str = json!({ "type": "string" });
    let int = json!({ "type": "integer" });
    let bool = json!({ "type": "boolean" });
    let id =
        json!({ "type": "string", "description": "Object id: 1-64 visible ASCII characters." });
    let token = json!({
        "type": "string",
        "description": "expected_state from your latest show or write of this object; object-v1:absent to create.",
    });
    let scope = json!({
        "type": "string",
        "description": "The Interest you work for; another's object is wrong-scope.",
    });
    let unbounded = |description: &str| {
        json!({
            "anyOf": [int, { "type": "string", "enum": ["unbounded"] }],
            "description": description,
        })
    };
    let question = json!({
        "type": "object",
        "properties": {
            "question": str, "true": str, "false": str, "guidance": str,
            "threshold": { "type": "number", "description": "Yes probability that passes; default 0.2." },
        },
        "required": ["question"],
        "additionalProperties": false,
    });
    let prompt = json!({ "type": "string", "description": "Timer: what to do when it fires." });
    let max_occurrences = unbounded("Timer: default 1.");
    let expires_at = unbounded("Timer: Unix time after which it never fires.");
    // The editable fields of a watch, as watch_add and watch_update take
    // them.
    let watch_fields = json!({
        "name": str,
        "enabled": bool,
        "since": {
            "anyOf": [int, { "type": "string", "enum": ["now"] }],
            "description": "Event watch: ignore events before this Unix time.",
        },
        "channels": {
            "anyOf": [{ "type": "string", "enum": ["all"] }, { "type": "array", "items": str }],
            "description": "Event watch: channel ids, or \"all\".",
        },
        "kinds": {
            "type": "array",
            "items": int,
            "description": "Event watch: event kinds; empty matches any.",
        },
        "tags": {
            "type": "object",
            "additionalProperties": { "type": "array", "items": str },
            "description": "Event watch: one-letter tag to accepted values.",
        },
        "filter": {
            "type": "string",
            "description": "Event watch: e.g. !(author == \"HEX\") && !is_reply. Also content == \"TEXT\", true, false.",
        },
        "classifier": {
            "type": "object",
            "properties": {
                "questions": {
                    "type": "object",
                    "additionalProperties": question,
                    "description": "Named yes/no questions; the event passes on any yes.",
                },
            },
            "required": ["questions"],
            "additionalProperties": false,
        },
        "prompt": prompt,
        "interval_secs": int,
        "max_occurrences": max_occurrences,
        "expires_at": expires_at,
    });
    let with = |base: Value, extra: &Value| {
        let mut base = base;
        for (key, value) in extra.as_object().expect("fields") {
            base[key] = value.clone();
        }
        base
    };
    let scoped = json!({ "id": id, "interest": scope, "expected_state": token });
    let mut tools = vec![
        tool(
            "interest_set",
            "Create an Interest, or replace its instructions.",
            json!({ "id": id, "instructions": str, "expected_state": token }),
            &["id", "instructions", "expected_state"],
        ),
        tool(
            "interest_show",
            "One Interest and its expected_state.",
            json!({ "id": id }),
            &["id"],
        ),
        tool(
            "interest_list",
            "Interest ids.",
            json!({ "search": str }),
            &[],
        ),
        tool(
            "interest_remove",
            "Remove an Interest that has no watches.",
            json!({ "id": id, "expected_state": token }),
            &["id", "expected_state"],
        ),
        tool(
            "watch_add",
            "Add an event watch (needs since and channels) or a timer (needs prompt and interval_secs) to an Interest.",
            with(
                json!({
                    "interest": { "type": "string", "description": "The Interest it serves." },
                    "id": id,
                    "type": { "type": "string", "enum": ["event", "timer"] },
                    "expected_state": token,
                }),
                &watch_fields,
            ),
            &["interest", "id", "type", "expected_state"],
        ),
        tool(
            "watch_update",
            "Change only the fields given; clear removes optional fields.",
            with(
                with(scoped.clone(), &watch_fields),
                &json!({
                    "clear": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["name", "tags", "filter", "classifier"] },
                    },
                }),
            ),
            &["id", "expected_state"],
        ),
    ];
    for (name, description) in [
        (
            "watch_enable",
            "Turn an event watch on. A timer is turned on by watch_rearm.",
        ),
        ("watch_disable", "Turn a watch off; it is kept."),
        ("watch_remove", "Remove a watch."),
    ] {
        tools.push(tool(
            name,
            description,
            scoped.clone(),
            &["id", "expected_state"],
        ));
    }
    tools.extend([
        tool(
            "watch_rearm",
            "Arm a timer, also one that is off: its next occurrence is one interval after at (default now, never earlier). Keeps its used occurrences; a spent timer needs higher limits first.",
            with(
                scoped,
                &json!({
                    "at": int,
                    "prompt": watch_fields["prompt"],
                    "interval_secs": int,
                    "max_occurrences": watch_fields["max_occurrences"],
                    "expires_at": watch_fields["expires_at"],
                }),
            ),
            &["id", "expected_state"],
        ),
        tool(
            "watch_show",
            "One watch and its expected_state.",
            json!({ "id": id, "interest": scope }),
            &["id"],
        ),
        tool(
            "watch_list",
            "Watch summaries.",
            json!({
                "interest": str,
                "type": { "type": "string", "enum": ["event", "timer"] },
                "status": {
                    "type": "string",
                    "enum": ["enabled", "disabled", "armed", "inactive", "spent"],
                },
                "search": { "type": "string", "description": "Case-sensitive, in ids and names." },
            }),
            &[],
        ),
    ]);
    tools
}

const UNAVAILABLE_NOTE: &str = "No classifier can run on this device now, so this watch's events pass unchecked. It is kept, and runs once a classifier is available.";

fn classifier_name(classifier: ClassifierAvailability) -> &'static str {
    match classifier {
        ClassifierAvailability::Available => "available",
        ClassifierAvailability::Unavailable => "unavailable",
    }
}

/// Runs attention tool `name`. An error is a tool error for the agent to
/// read; its text names the classifier state too.
pub async fn call_attention_tool(
    attention: &dyn Attention,
    name: &str,
    args: &Args,
) -> Result<String, String> {
    let classifier = attention.classifier();
    let named = classifier_name(classifier);
    match run(attention, name, args, classifier).await {
        Ok(answer) => {
            let mut object = Map::new();
            object.insert("classifier".into(), json!(named));
            if let Value::Object(answer) = answer {
                object.extend(answer);
            }
            Ok(pretty(&Value::Object(object)))
        }
        Err(error) => Err(format!("{error}\n(classifier: {named})")),
    }
}

/// `JSON.stringify(value, null, 1)`.
fn pretty(value: &Value) -> String {
    use serde::Serialize;
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    value
        .serialize(&mut serializer)
        .expect("JSON values serialize");
    String::from_utf8(out).expect("JSON is UTF-8")
}

/// A refused call, as Janet reports a well-formed call this object does
/// not allow.
fn refuse<T>(message: impl std::fmt::Display) -> Result<T, String> {
    Err(format!("invalid-operation: {message}"))
}

fn fail<T>(message: impl std::fmt::Display) -> Result<T, String> {
    Err(format!("invalid: {message}"))
}

/// A trimmed, non-empty string argument.
fn text(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn required(args: &Args, name: &str) -> Result<String, String> {
    match args.get(name) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ if name == "expected_state" => refuse("every write needs the expected_state of a show"),
        _ => fail(format!("{name} is required")),
    }
}

/// `String(args.id)` for messages.
fn shown_id(args: &Args) -> String {
    match args.get("id") {
        Some(Value::String(id)) => id.clone(),
        Some(other) => other.to_string(),
        None => "undefined".into(),
    }
}

const EVENT: [&str; 8] = [
    "name",
    "enabled",
    "since",
    "channels",
    "kinds",
    "tags",
    "filter",
    "classifier",
];
const TIMER: [&str; 5] = [
    "enabled",
    "prompt",
    "interval_secs",
    "max_occurrences",
    "expires_at",
];

fn pick(args: &Args, names: &[&str], other: &[&str]) -> Result<Args, String> {
    let wrong: Vec<&str> = other
        .iter()
        .filter(|key| !names.contains(key) && args.contains_key(**key))
        .copied()
        .collect();
    if !wrong.is_empty() {
        return fail(format!(
            "Not a field of this watch type: {}",
            wrong.join(", ")
        ));
    }
    Ok(names
        .iter()
        .filter_map(|key| {
            args.get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect())
}

fn event_fields(args: &Args, now: u64) -> Result<Args, String> {
    let mut fields = pick(args, &EVENT, &TIMER)?;
    if fields.get("since") == Some(&json!("now")) {
        fields.insert("since".into(), json!(now));
    }
    Ok(fields)
}

fn timer_fields(args: &Args) -> Result<Args, String> {
    let mut fields = pick(args, &TIMER, &EVENT)?;
    for key in ["max_occurrences", "expires_at"] {
        if fields.get(key) == Some(&json!("unbounded")) {
            fields.insert(key.into(), Value::Null);
        }
    }
    Ok(fields)
}

fn merged(value: Value, fields: Args) -> Args {
    let mut next = match value {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    next.extend(fields);
    next
}

async fn run(
    attention: &dyn Attention,
    name: &str,
    args: &Args,
    classifier: ClassifierAvailability,
) -> Result<Value, String> {
    // A show or write as the tools answer it, with a note when its
    // watch's classifier cannot run.
    let shown = |shown: Shown| {
        let mut answer = shown.to_json();
        let classified = shown.object.as_ref().is_some_and(|object| {
            matches!(object.value(), crate::agents2::attention::AttentionValue::Event(watch) if watch.classifier.is_some())
        });
        if classifier == ClassifierAvailability::Unavailable && classified {
            answer["classifier_note"] = json!(UNAVAILABLE_NOTE);
        }
        answer
    };
    let scoped = text(args.get("interest"));
    let watch = |key: &str| required(args, key).map(|id| format!("watch/{id}"));
    let options = |rearm: bool| -> Result<WriteOptions, String> {
        Ok(WriteOptions {
            expected: required(args, "expected_state")?,
            interest: scoped.clone(),
            rearm,
        })
    };
    let now = attention.now();
    match name {
        "interest_set" => {
            let slug = format!("interest/{}", required(args, "id")?);
            let value =
                json!({ "type": "interest", "instructions": required(args, "instructions")? });
            let expected = required(args, "expected_state")?;
            let options = WriteOptions {
                expected,
                ..Default::default()
            };
            Ok(attention.write(slug, Some(value), options).await?.to_json())
        }
        "interest_show" => {
            let slug = format!("interest/{}", required(args, "id")?);
            Ok(attention.show(slug, None).await?.to_json())
        }
        "interest_list" => {
            let search = text(args.get("search")).unwrap_or_default();
            Ok(json!({ "interests": attention.interests(search).await? }))
        }
        "interest_remove" => {
            let slug = format!("interest/{}", required(args, "id")?);
            let options = WriteOptions {
                expected: required(args, "expected_state")?,
                ..Default::default()
            };
            Ok(attention.write(slug, None, options).await?.to_json())
        }
        "watch_add" => {
            let interest = required(args, "interest")?;
            let slug = watch("id")?;
            // As in Janet, adding never replaces: an existing watch is
            // changed with watch_update, which keeps the fields it is not
            // given.
            if attention.show(slug.clone(), None).await?.object.is_some() {
                return refuse(format!("watch {} exists; use watch_update", shown_id(args)));
            }
            let value = match args.get("type").and_then(Value::as_str) {
                Some("timer") => merged(
                    json!({
                        "type": "timer", "interest_id": interest, "enabled": true,
                        "armed_at": now, "max_occurrences": 1, "expires_at": null,
                    }),
                    timer_fields(args)?,
                ),
                Some("event") => merged(
                    json!({ "type": "event", "interest_id": interest, "enabled": true, "kinds": [] }),
                    event_fields(args, now)?,
                ),
                _ => return fail("type must be \"event\" or \"timer\""),
            };
            let options = WriteOptions {
                expected: required(args, "expected_state")?,
                interest: Some(interest),
                rearm: false,
            };
            Ok(shown(
                attention
                    .write(slug, Some(Value::Object(value)), options)
                    .await?,
            ))
        }
        "watch_update" | "watch_enable" | "watch_disable" | "watch_rearm" => {
            let at = match (name, args.get("at")) {
                ("watch_rearm", Some(at)) => match at.as_f64() {
                    Some(at) if at >= now as f64 => Some(at),
                    _ => return fail("at must be a Unix time no earlier than now"),
                },
                _ => None,
            };
            let slug = watch("id")?;
            let current = attention.show(slug.clone(), scoped.clone()).await?.object;
            let Some(current) = current.filter(|object| !object.value().is_interest()) else {
                return Err(format!("not-found: no watch {}", shown_id(args)));
            };
            let used = match &current {
                crate::agents2::objects::ShownObject::Timer { used, .. } => *used,
                _ => 0,
            };
            let value = current.value();
            let timer = matches!(value, crate::agents2::attention::AttentionValue::Timer(_));
            let stored = value.to_json();
            let next = match name {
                "watch_update" => {
                    let fields = if timer {
                        timer_fields(args)?
                    } else {
                        event_fields(args, now)?
                    };
                    let clear: Vec<String> = args
                        .get("clear")
                        .and_then(Value::as_array)
                        .map(|clear| {
                            clear
                                .iter()
                                .map(|key| {
                                    key.as_str().map_or_else(|| key.to_string(), str::to_owned)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if timer && !clear.is_empty() {
                        return refuse("a timer has no optional fields to clear");
                    }
                    // Janet's rule: only a rearm arms a timer, with a new
                    // deadline.
                    if timer
                        && stored["enabled"] == json!(false)
                        && fields.get("enabled") == Some(&json!(true))
                    {
                        return refuse("use watch_rearm to turn a timer on");
                    }
                    let mut next = merged(stored, fields);
                    for key in clear {
                        next.remove(&key);
                    }
                    next
                }
                "watch_enable" | "watch_disable" => {
                    // Janet's rule: only a rearm arms a timer, with a new
                    // deadline.
                    if timer && name == "watch_enable" {
                        return refuse("use watch_rearm to turn a timer on");
                    }
                    merged(
                        stored,
                        Map::from_iter([("enabled".to_owned(), json!(name == "watch_enable"))]),
                    )
                }
                _ => {
                    if !timer {
                        return refuse("only a timer can be rearmed");
                    }
                    let mut rest = args.clone();
                    rest.remove("at");
                    let mut next = merged(stored, timer_fields(&rest)?);
                    // Janet's rule: rearming never resets usage, so a spent
                    // timer stays spent until its limits are raised.
                    let reached = |key: &str, past: bool| {
                        next.get(key).and_then(Value::as_f64).is_some_and(|limit| {
                            if past {
                                now as f64 >= limit
                            } else {
                                used as f64 >= limit
                            }
                        })
                    };
                    if reached("max_occurrences", false) || reached("expires_at", true) {
                        return refuse(
                            "the timer is spent; raise max_occurrences or expires_at to rearm it",
                        );
                    }
                    next.insert("enabled".into(), json!(true));
                    next.insert("armed_at".into(), at.map_or(json!(now), |at| json!(at)));
                    next
                }
            };
            let options = options(name == "watch_rearm")?;
            Ok(shown(
                attention
                    .write(slug, Some(Value::Object(next)), options)
                    .await?,
            ))
        }
        "watch_remove" => {
            let slug = watch("id")?;
            Ok(attention
                .write(slug, None, options(false)?)
                .await?
                .to_json())
        }
        "watch_show" => {
            let slug = watch("id")?;
            Ok(shown(attention.show(slug, scoped).await?))
        }
        "watch_list" => {
            let status = match args.get("status").and_then(Value::as_str) {
                None => None,
                Some(status) => match serde_json::from_value::<WatchStatus>(json!(status)) {
                    Ok(status) => Some(status),
                    // A status no watch has.
                    Err(_) => return Ok(json!({ "watches": [] })),
                },
            };
            let filter = WatchFilter {
                interest: scoped,
                kind: args
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|kind| ["event", "timer"].contains(kind))
                    .map(str::to_owned),
                status,
                search: text(args.get("search")),
            };
            let watches: Vec<Value> = attention
                .watches(filter)
                .await?
                .into_iter()
                .map(|summary| {
                    let classified = summary.classified;
                    let mut value = serde_json::to_value(summary).expect("summaries serialize");
                    if classified && classifier == ClassifierAvailability::Unavailable {
                        value["classifier"] = json!("unavailable");
                    }
                    value
                })
                .collect();
            Ok(json!({ "watches": watches }))
        }
        _ => Err(format!("No tool {name}")),
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
