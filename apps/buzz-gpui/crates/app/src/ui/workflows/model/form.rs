//! Form state and its YAML (`workflowFormTypes.ts`). Form mode edits the
//! definitions it can write back without loss: anything else is refused
//! with a reason that ends "use the YAML editor", and the YAML stays as
//! written.

use super::conditions::Condition;
use super::cron;
use super::duration;
use super::yaml::{self, get, key_text};
use serde_yaml::{Mapping, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerType {
    MessagePosted,
    ReactionAdded,
    DiffPosted,
    Schedule,
    Webhook,
}

pub const TRIGGER_TYPES: [TriggerType; 5] = [
    TriggerType::MessagePosted,
    TriggerType::ReactionAdded,
    TriggerType::DiffPosted,
    TriggerType::Schedule,
    TriggerType::Webhook,
];

impl TriggerType {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "message_posted" => Self::MessagePosted,
            "reaction_added" => Self::ReactionAdded,
            "diff_posted" => Self::DiffPosted,
            "schedule" => Self::Schedule,
            "webhook" => Self::Webhook,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MessagePosted => "message_posted",
            Self::ReactionAdded => "reaction_added",
            Self::DiffPosted => "diff_posted",
            Self::Schedule => "schedule",
            Self::Webhook => "webhook",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MessagePosted => "Message posted",
            Self::ReactionAdded => "Reaction added",
            Self::DiffPosted => "Diff posted",
            Self::Schedule => "Schedule",
            Self::Webhook => "Webhook",
        }
    }

    /// The relay accepts `reply_in_thread` only where a message triggered
    /// the run.
    pub fn thread_reply_eligible(self) -> bool {
        !matches!(self, Self::Schedule | Self::Webhook)
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::MessagePosted | Self::DiffPosted => &["on", "filter"],
            Self::ReactionAdded => &["on", "emoji", "filter"],
            Self::Schedule => &["on", "cron", "interval"],
            Self::Webhook => &["on"],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionType {
    Delay,
    SendMessage,
    CallWebhook,
}

/// In the order the step action select lists them.
pub const ACTION_TYPES: [ActionType; 3] = [
    ActionType::Delay,
    ActionType::SendMessage,
    ActionType::CallWebhook,
];

impl ActionType {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "delay" => Self::Delay,
            "send_message" => Self::SendMessage,
            "call_webhook" => Self::CallWebhook,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delay => "delay",
            Self::SendMessage => "send_message",
            Self::CallWebhook => "call_webhook",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Delay => "Delay",
            Self::SendMessage => "Send Message",
            Self::CallWebhook => "Call Webhook",
        }
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::CallWebhook => &["url", "method", "headers", "body"],
            Self::Delay => &["duration"],
            Self::SendMessage => &["text", "channel", "reply_in_thread"],
        }
    }

    fn required(self) -> &'static [&'static str] {
        match self {
            Self::CallWebhook => &["url"],
            Self::Delay => &["duration"],
            Self::SendMessage => &["text"],
        }
    }

    fn optional(self) -> &'static [&'static str] {
        match self {
            Self::CallWebhook => &["method", "body"],
            Self::Delay => &[],
            Self::SendMessage => &["channel"],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerConfig {
    pub on: TriggerType,
    pub filter: Option<String>,
    /// Unsaved Basic rows, incomplete ones included; never serialized.
    pub condition_rows: Option<Vec<Condition>>,
    pub emoji: Option<String>,
    pub cron: Option<String>,
    pub interval: Option<String>,
}

impl TriggerConfig {
    pub fn new(on: TriggerType) -> Self {
        Self {
            on,
            filter: None,
            condition_rows: None,
            emoji: None,
            cron: None,
            interval: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub id: String,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub name: Option<String>,
    pub action: ActionType,
    pub timeout_secs: Option<String>,
    pub duration: Option<String>,
    pub text: Option<String>,
    pub channel: Option<String>,
    pub reply_in_thread: Option<bool>,
    pub url: Option<String>,
    pub method: Option<String>,
    pub headers: Option<Vec<Header>>,
    pub body: Option<String>,
    pub condition: Option<String>,
}

impl Step {
    /// A new step with the action's starting value: empty text, a 5m
    /// delay, or an empty URL.
    pub fn new(id: String, action: ActionType) -> Self {
        let mut step = Self {
            id,
            name: None,
            action,
            timeout_secs: None,
            duration: None,
            text: None,
            channel: None,
            reply_in_thread: None,
            url: None,
            method: None,
            headers: None,
            body: None,
            condition: None,
        };
        step.seed();
        step
    }

    fn seed(&mut self) {
        match self.action {
            ActionType::SendMessage => self.text = Some(String::new()),
            ActionType::Delay => self.duration = Some("5m".into()),
            ActionType::CallWebhook => self.url = Some(String::new()),
        }
    }

    /// Changes the action, keeping only the id, name, timeout and
    /// condition.
    pub fn with_action(&self, action: ActionType) -> Self {
        let mut step = Self::new(self.id.clone(), action);
        step.name = self.name.clone();
        step.timeout_secs = self.timeout_secs.clone();
        step.condition = self.condition.clone();
        step
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowForm {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub trigger: TriggerConfig,
    pub steps: Vec<Step>,
}

/// New drafts start disabled.
impl Default for WorkflowForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            enabled: false,
            trigger: TriggerConfig::new(TriggerType::MessagePosted),
            steps: Vec::new(),
        }
    }
}

/// The YAML a new draft opens with.
pub fn new_draft_yaml() -> String {
    form_to_yaml(&WorkflowForm {
        name: "Untitled workflow".into(),
        ..Default::default()
    })
}

/// A blank timeout is omitted; a valid one is seconds; invalid text is
/// kept so validation refuses it rather than saving no timeout.
fn timeout_value(timeout: Option<&str>) -> Option<Value> {
    let timeout = timeout.filter(|t| !t.trim().is_empty())?;
    Some(match duration::parse_seconds(timeout).filter(|s| *s > 0) {
        Some(seconds) => Value::Number(seconds.into()),
        None => Value::String(timeout.into()),
    })
}

fn put(map: &mut Mapping, key: &str, value: Value) {
    map.insert(Value::String(key.into()), value);
}

fn text(value: &str) -> Value {
    Value::String(value.into())
}

fn step_value(step: &Step) -> Value {
    let mut map = Mapping::new();
    put(&mut map, "id", text(&step.id));
    put(&mut map, "action", text(step.action.as_str()));
    if let Some(condition) = &step.condition {
        put(&mut map, "if", text(condition));
    }
    if let Some(name) = step
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        put(&mut map, "name", text(name));
    }
    if let Some(timeout) = timeout_value(step.timeout_secs.as_deref()) {
        put(&mut map, "timeout_secs", timeout);
    }
    let some = |value: &Option<String>| value.as_deref().filter(|v| !v.is_empty()).map(text);
    match step.action {
        ActionType::CallWebhook => {
            if let Some(url) = some(&step.url) {
                put(&mut map, "url", url);
            }
            if let Some(method) = some(&step.method) {
                put(&mut map, "method", method);
            }
            if let Some(body) = some(&step.body) {
                put(&mut map, "body", body);
            }
            if let Some(headers) = step.headers.as_ref().filter(|h| !h.is_empty()) {
                let mut values = Mapping::new();
                for header in headers {
                    put(&mut values, &header.name, text(&header.value));
                }
                put(&mut map, "headers", Value::Mapping(values));
            }
        }
        ActionType::Delay => {
            if let Some(duration) = some(&step.duration) {
                put(&mut map, "duration", duration);
            }
        }
        ActionType::SendMessage => {
            if let Some(message) = some(&step.text) {
                put(&mut map, "text", message);
            }
            if let Some(channel) = some(&step.channel) {
                put(&mut map, "channel", channel);
            }
            if step.reply_in_thread == Some(true) {
                put(&mut map, "reply_in_thread", Value::Bool(true));
            }
        }
    }
    Value::Mapping(map)
}

/// Writes form state as YAML. Form changes may reformat it.
pub fn form_to_yaml(form: &WorkflowForm) -> String {
    let mut trigger = Mapping::new();
    put(&mut trigger, "on", text(form.trigger.on.as_str()));
    let set = |value: &Option<String>| value.as_deref().filter(|v| !v.is_empty()).map(text);
    match form.trigger.on {
        // The relay refuses cron and interval together; write exactly one.
        TriggerType::Schedule => {
            if let Some(cron) = set(&form.trigger.cron) {
                put(&mut trigger, "cron", cron);
            } else if let Some(interval) = set(&form.trigger.interval) {
                put(&mut trigger, "interval", interval);
            }
        }
        // A webhook carries only `on`; the relay owns its URL and secret.
        TriggerType::Webhook => {}
        on => {
            if let Some(filter) = set(&form.trigger.filter) {
                put(&mut trigger, "filter", filter);
            }
            if on == TriggerType::ReactionAdded
                && let Some(emoji) = set(&form.trigger.emoji)
            {
                put(&mut trigger, "emoji", emoji);
            }
        }
    }
    let mut workflow = Mapping::new();
    put(&mut workflow, "name", text(&form.name));
    put(&mut workflow, "trigger", Value::Mapping(trigger));
    put(
        &mut workflow,
        "steps",
        Value::Sequence(form.steps.iter().map(step_value).collect()),
    );
    if !form.description.trim().is_empty() {
        put(&mut workflow, "description", text(form.description.trim()));
    }
    if !form.enabled {
        put(&mut workflow, "enabled", Value::Bool(false));
    }
    yaml::stringify(&Value::Mapping(workflow))
}

/// Changing the trigger clears threaded replies on every step when the new
/// one has no message to reply to, so a hidden flag cannot come back.
pub fn with_trigger_type(form: &WorkflowForm, on: TriggerType) -> WorkflowForm {
    WorkflowForm {
        trigger: TriggerConfig::new(on),
        steps: if on.thread_reply_eligible() {
            form.steps.clone()
        } else {
            form.steps
                .iter()
                .map(|step| {
                    let mut step = step.clone();
                    if step.reply_in_thread == Some(true) {
                        step.reply_in_thread = Some(false);
                    }
                    step
                })
                .collect()
        },
        ..form.clone()
    }
}

/// The first free `step_N`.
pub fn next_step_id(steps: &[Step]) -> String {
    let taken: std::collections::HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    (1..)
        .map(|n| format!("step_{n}"))
        .find(|id| !taken.contains(id.as_str()))
        .expect("an unused step id")
}

/// `String(value)` for an error message.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(value)) => value.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Sequence(_)) => String::new(),
        Some(Value::Mapping(_)) => "[object Object]".into(),
        Some(Value::Tagged(tagged)) => js_string(Some(&tagged.value)),
    }
}

fn unknown_key(map: &Mapping, allowed: &[&str]) -> Option<String> {
    map.keys()
        .map(key_text)
        .find(|key| !allowed.contains(&key.as_str()))
}

/// `Number.isSafeInteger`.
fn safe_integer(value: &Value) -> Option<i64> {
    let Value::Number(number) = value else {
        return None;
    };
    if let Some(int) = number.as_i64() {
        return (int.unsigned_abs() <= duration::SAFE).then_some(int);
    }
    if number.as_u64().is_some() {
        return None;
    }
    let float = number.as_f64()?;
    (float.fract() == 0.0 && float.abs() <= duration::SAFE as f64).then_some(float as i64)
}

fn optional_string_error(map: &Mapping, key: &str, label: &str) -> Option<String> {
    match get(map, key)? {
        Value::String(value) if value.is_empty() => Some(format!(
            "{label} cannot be empty in Form mode — use the YAML editor"
        )),
        Value::String(_) => None,
        _ => Some(format!("{label} must be a string")),
    }
}

fn optional_text(map: &Mapping, key: &str) -> Option<String> {
    get(map, key).and_then(Value::as_str).map(str::to_owned)
}

/// Reads form state from YAML, or why Form mode cannot show it.
pub fn yaml_to_form(text: &str) -> Result<WorkflowForm, String> {
    let parsed = yaml::parse(text)?;
    let Value::Mapping(parsed) = parsed else {
        return Err("YAML must be an object".into());
    };
    if let Some(key) = unknown_key(
        &parsed,
        &["name", "description", "enabled", "trigger", "steps"],
    ) {
        return Err(format!(
            "Unsupported workflow field \"{key}\" — use the YAML editor"
        ));
    }
    let Some(name) = get(&parsed, "name").and_then(Value::as_str) else {
        return Err("name must be a string".into());
    };
    let description = match get(&parsed, "description") {
        None => String::new(),
        Some(Value::String(description)) => {
            if description.is_empty() || description.trim() != description {
                return Err("description cannot be empty or have surrounding whitespace in Form mode — use the YAML editor".into());
            }
            description.clone()
        }
        Some(_) => return Err("description must be a string".into()),
    };
    let enabled = match get(&parsed, "enabled") {
        None => true,
        Some(Value::Bool(enabled)) => *enabled,
        Some(_) => return Err("enabled must be a boolean".into()),
    };
    let trigger = get(&parsed, "trigger").and_then(Value::as_mapping);
    let Some((raw, on)) = trigger.and_then(|t| Some((t, get(t, "on")?.as_str()?))) else {
        return Err("trigger.on is required".into());
    };
    let Some(on) = TriggerType::parse(on) else {
        return Err(format!(
            "Unsupported trigger type \"{on}\" — use the YAML editor"
        ));
    };
    if let Some(key) = unknown_key(raw, on.keys()) {
        return Err(format!(
            "Unsupported {} trigger field \"{key}\" — use the YAML editor",
            on.as_str()
        ));
    }
    for key in ["filter", "emoji", "cron", "interval"] {
        if let Some(error) = optional_string_error(raw, key, &format!("trigger.{key}")) {
            // Schedule fields stay editable in YAML; say so for type errors too.
            return Err(
                if on == TriggerType::Schedule && !error.contains("YAML editor") {
                    format!("{error} — use the YAML editor")
                } else {
                    error
                },
            );
        }
    }
    if on == TriggerType::Schedule {
        let has_cron = get(raw, "cron").is_some();
        if has_cron == get(raw, "interval").is_some() {
            return Err(if has_cron {
                "Schedule triggers cannot specify both cron and interval — use the YAML editor"
            } else {
                "Schedule triggers require either cron or interval — use the YAML editor"
            }
            .into());
        }
        if let Some(error) = get(raw, "cron")
            .and_then(Value::as_str)
            .and_then(cron::expression_error)
        {
            return Err(format!(
                "Unsupported cron expression: {error} Use the YAML editor"
            ));
        }
    }
    let trigger = TriggerConfig {
        on,
        filter: optional_text(raw, "filter"),
        condition_rows: None,
        emoji: optional_text(raw, "emoji"),
        cron: optional_text(raw, "cron"),
        interval: optional_text(raw, "interval"),
    };
    let Some(raw_steps) = get(&parsed, "steps").and_then(Value::as_sequence) else {
        return Err("steps must be a list".into());
    };
    let mut ids = std::collections::HashSet::new();
    let mut steps = Vec::with_capacity(raw_steps.len());
    for (index, value) in raw_steps.iter().enumerate() {
        let number = index + 1;
        let Some(step) = value.as_mapping() else {
            return Err(format!("Step {number} must be an object"));
        };
        let Some(id) = get(step, "id")
            .and_then(Value::as_str)
            .filter(|id| buzz_relay_client::workflows::protocol::step_id(id))
        else {
            return Err(format!(
                "Step {number} requires a unique 1–64 character alphanumeric or underscore ID"
            ));
        };
        if !ids.insert(id.to_string()) {
            return Err(format!("Duplicate step ID \"{id}\" — use the YAML editor"));
        }
        let action = get(step, "action");
        let Some(action) = action.and_then(Value::as_str).and_then(ActionType::parse) else {
            return Err(format!(
                "Unsupported action type \"{}\" — use the YAML editor",
                js_string(action)
            ));
        };
        let allowed: Vec<&str> = ["id", "name", "action", "if", "timeout_secs"]
            .into_iter()
            .chain(action.keys().iter().copied())
            .collect();
        if let Some(key) = unknown_key(step, &allowed) {
            return Err(format!(
                "Unsupported {} step field \"{key}\" — use the YAML editor",
                action.as_str()
            ));
        }
        if get(step, "if").is_some_and(|value| !value.is_string()) {
            return Err("Step conditions must be strings — use the YAML editor".into());
        }
        if let Some(error) = optional_string_error(step, "name", &format!("Step {number} name")) {
            return Err(error);
        }
        if get(step, "name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.trim() != name)
        {
            return Err(format!(
                "Step {number} name has surrounding whitespace — use the YAML editor"
            ));
        }
        let timeout = match get(step, "timeout_secs") {
            None => None,
            Some(value) => match safe_integer(value).filter(|seconds| *seconds > 0) {
                Some(seconds) => Some(duration::format_seconds(seconds as u64)),
                None => {
                    return Err(format!(
                        "Step {number} timeout_secs must be a positive integer"
                    ));
                }
            },
        };
        for key in action.required() {
            if !get(step, key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
            {
                return Err(format!("Step {number} {key} must be a non-empty string"));
            }
        }
        for key in action.optional() {
            if let Some(error) = optional_string_error(step, key, &format!("Step {number} {key}")) {
                return Err(error);
            }
        }
        if let Some(reply) = get(step, "reply_in_thread") {
            let Some(reply) = reply.as_bool() else {
                return Err(format!(
                    "Step {number} reply_in_thread must be a boolean — use the YAML editor"
                ));
            };
            // An explicit false is accepted anywhere.
            if reply && !on.thread_reply_eligible() {
                return Err(format!(
                    "reply_in_thread is not supported for {} triggers — use the YAML editor",
                    on.as_str()
                ));
            }
        }
        let headers = match (action, get(step, "headers")) {
            (ActionType::CallWebhook, Some(headers)) => {
                let map = headers
                    .as_mapping()
                    .filter(|map| map.values().all(Value::is_string))
                    .ok_or_else(|| {
                        "Webhook headers must be a string map — use the YAML editor".to_string()
                    })?;
                Some(
                    map.iter()
                        .enumerate()
                        .map(|(index, (name, value))| Header {
                            id: format!("header_{index}"),
                            name: key_text(name),
                            value: value.as_str().unwrap_or_default().to_string(),
                        })
                        .collect(),
                )
            }
            _ => None,
        };
        steps.push(Step {
            id: id.to_string(),
            name: optional_text(step, "name"),
            action,
            condition: optional_text(step, "if"),
            url: optional_text(step, "url"),
            method: optional_text(step, "method"),
            body: optional_text(step, "body"),
            headers,
            timeout_secs: timeout,
            duration: optional_text(step, "duration"),
            text: optional_text(step, "text"),
            channel: optional_text(step, "channel"),
            reply_in_thread: Some(get(step, "reply_in_thread") == Some(&Value::Bool(true))),
        });
    }
    Ok(WorkflowForm {
        name: name.to_string(),
        description,
        enabled,
        trigger,
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn accepted(text: &str) -> WorkflowForm {
        yaml_to_form(text).unwrap_or_else(|error| panic!("{error}: {text}"))
    }

    /// The relay reads a missing `enabled` as true.
    fn normalized(text: &str) -> Value {
        let mut value = yaml::parse(text).unwrap();
        if let Value::Mapping(map) = &mut value
            && get(map, "enabled").is_none()
        {
            put(map, "enabled", Value::Bool(true));
        }
        value
    }

    const ACCEPTED: [&str; 8] = [
        "name: Notify\ntrigger: { on: message_posted }\nsteps: [{ id: notify_1, action: send_message, text: hello }]\n",
        "name: React\ndescription: Reply to a reaction\nenabled: false\ntrigger: { on: reaction_added, emoji: eyes, filter: 'trigger_message_id == \"abc123\"' }\nsteps: [{ id: reply, name: Reply, timeout_secs: 30, action: send_message, text: hi, channel: channel-id, reply_in_thread: true }, { id: wait, action: delay, duration: 5m }]\n",
        "name: Diff\ntrigger: { on: diff_posted }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
        "name: Diff review\ntrigger: { on: diff_posted, filter: 'str_contains(trigger_text, \"src/\")' }\nsteps: [{ id: s1, action: send_message, text: reviewing, reply_in_thread: true }, { id: wait, action: delay, duration: 1m }]\n",
        "name: Standup\ntrigger: { on: schedule, cron: '0 9 * * 1-5' }\nsteps: [{ id: prompt, action: send_message, text: Standup time }]\n",
        "name: Tick\nenabled: false\ntrigger: { on: schedule, interval: 30m }\nsteps: [{ id: s1, action: send_message, text: tick }, { id: wait, action: delay, duration: 1m }]\n",
        "name: Aliases\ntrigger: { on: schedule, cron: '0 */2 1,15 JAN,MAR MON-FRI' }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
        "name: Hook\ntrigger: { on: webhook }\nsteps: [{ id: s1, action: send_message, text: hi }, { id: wait, action: delay, duration: 2s }]\n",
    ];

    #[test]
    fn accepted_fixtures_survive_a_semantic_round_trip() {
        for fixture in ACCEPTED {
            let generated = form_to_yaml(&accepted(fixture));
            assert_eq!(normalized(&generated), normalized(fixture), "{generated}");
            // And the generated YAML reads back to the same form.
            assert_eq!(accepted(&generated), accepted(fixture));
        }
    }

    #[test]
    fn unknown_fields_are_refused_for_the_yaml_editor() {
        for text in [
            "name: Test\nunknown: true\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: message_posted, future_filter: x }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message, text: hi, retry: 3 }]\n",
            "name: Test\ntrigger: { on: diff_posted, cron: '0 9 * * *' }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: schedule, cron: '0 9 * * *', filter: 'true' }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: schedule, cron: '0 9 * * *', emoji: eyes }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
        ] {
            assert!(
                yaml_to_form(text).unwrap_err().contains("YAML editor"),
                "{text}"
            );
        }
        assert_eq!(
            yaml_to_form("name: Test\ntrigger: { on: diff_posted, emoji: eyes }\nsteps: [{ id: s1, action: send_message, text: hi }]\n").unwrap_err(),
            "Unsupported diff_posted trigger field \"emoji\" — use the YAML editor"
        );
    }

    #[test]
    fn writes_only_the_trigger_fields_its_type_carries() {
        let trigger = |config: TriggerConfig| {
            let text = form_to_yaml(&WorkflowForm {
                name: "T".into(),
                trigger: config,
                steps: vec![Step {
                    text: Some("hi".into()),
                    ..Step::new("s1".into(), ActionType::SendMessage)
                }],
                ..Default::default()
            });
            yaml::parse(&text).unwrap()["trigger"].clone()
        };
        let mut diff = TriggerConfig::new(TriggerType::DiffPosted);
        diff.emoji = Some("eyes".into());
        diff.filter = Some("trigger_text != \"\"".into());
        assert_eq!(
            trigger(diff),
            yaml::parse("on: diff_posted\nfilter: 'trigger_text != \"\"'").unwrap()
        );
        let mut both = TriggerConfig::new(TriggerType::Schedule);
        both.cron = Some("0 9 * * *".into());
        both.interval = Some("15m".into());
        assert_eq!(
            trigger(both),
            yaml::parse("{on: schedule, cron: '0 9 * * *'}").unwrap()
        );
        let mut empty_cron = TriggerConfig::new(TriggerType::Schedule);
        empty_cron.cron = Some(String::new());
        empty_cron.interval = Some("1h".into());
        assert_eq!(
            trigger(empty_cron),
            yaml::parse("{on: schedule, interval: 1h}").unwrap()
        );
        let mut message = TriggerConfig::new(TriggerType::MessagePosted);
        message.cron = Some("0 9 * * *".into());
        assert_eq!(
            trigger(message),
            yaml::parse("{on: message_posted}").unwrap()
        );
        let mut hook = TriggerConfig::new(TriggerType::Webhook);
        hook.filter = Some("stale".into());
        hook.emoji = Some("eyes".into());
        assert_eq!(trigger(hook), yaml::parse("{on: webhook}").unwrap());
    }

    #[test]
    fn threaded_replies_follow_the_trigger() {
        for on in ["message_posted", "reaction_added", "diff_posted"] {
            let form = accepted(&format!(
                "name: Reply\ntrigger: {{ on: {on} }}\nsteps: [{{ id: s1, action: send_message, text: hi, reply_in_thread: true }}]\n"
            ));
            assert_eq!(form.steps[0].reply_in_thread, Some(true));
            assert!(form_to_yaml(&form).contains("reply_in_thread: true"));
        }
        assert_eq!(
            yaml_to_form("name: Bad\ntrigger: { on: schedule, cron: '0 9 * * 1-5' }\nsteps: [{ id: s1, action: send_message, text: hi, reply_in_thread: true }]\n").unwrap_err(),
            "reply_in_thread is not supported for schedule triggers — use the YAML editor"
        );
        let explicit = accepted(
            "name: OK\ntrigger: { on: webhook }\nsteps: [{ id: s1, action: send_message, text: hi, reply_in_thread: false }]\n",
        );
        assert_eq!(explicit.steps[0].reply_in_thread, Some(false));
        assert_eq!(explicit.trigger, TriggerConfig::new(TriggerType::Webhook));
        assert!(
            yaml_to_form("name: C\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message, text: hi, reply_in_thread: \"yes\" }]\n")
                .unwrap_err()
                .contains("reply_in_thread must be a boolean")
        );
    }

    #[test]
    fn schedules_need_exactly_one_valid_representation() {
        let refused = |trigger: &str| {
            yaml_to_form(&format!(
                "name: Test\ntrigger: {trigger}\nsteps: [{{ id: s1, action: send_message, text: hi }}]\n"
            ))
            .unwrap_err()
        };
        assert_eq!(
            refused("{ on: schedule, cron: '0 9 * * *', interval: 30m }"),
            "Schedule triggers cannot specify both cron and interval — use the YAML editor"
        );
        assert_eq!(
            refused("{ on: schedule }"),
            "Schedule triggers require either cron or interval — use the YAML editor"
        );
        assert_eq!(
            refused("{ on: schedule, cron: not-a-cron }"),
            "Unsupported cron expression: Paste a 5-field cron expression. Found 1 field. Use the YAML editor"
        );
        assert_eq!(
            refused("{ on: schedule, cron: '0 9 * * 8' }"),
            "Unsupported cron expression: Weekday must be between 1 and 7. Use the YAML editor"
        );
        assert_eq!(
            refused("{ on: schedule, cron: '0 0 9 * * 1-5' }"),
            "Unsupported cron expression: Paste a 5-field cron expression. Found 6 fields. Use the YAML editor"
        );
        assert_eq!(
            refused("{ on: schedule, cron: '' }"),
            "trigger.cron cannot be empty in Form mode — use the YAML editor"
        );
        assert_eq!(
            refused("{ on: schedule, interval: 30 }"),
            "trigger.interval must be a string — use the YAML editor"
        );
        assert_eq!(
            refused("{ on: message_posted, filter: 42 }"),
            "trigger.filter must be a string"
        );
    }

    #[test]
    fn switching_to_a_schedule_clears_threaded_replies() {
        let mut form = WorkflowForm {
            name: "Switch".into(),
            ..Default::default()
        };
        form.steps = vec![
            Step {
                reply_in_thread: Some(true),
                ..Step::new("s1".into(), ActionType::SendMessage)
            },
            Step {
                reply_in_thread: Some(true),
                ..Step::new("s2".into(), ActionType::Delay)
            },
            Step::new("s3".into(), ActionType::SendMessage),
        ];
        let scheduled = with_trigger_type(&form, TriggerType::Schedule);
        assert_eq!(scheduled.trigger, TriggerConfig::new(TriggerType::Schedule));
        assert_eq!(
            scheduled
                .steps
                .iter()
                .map(|s| s.reply_in_thread)
                .collect::<Vec<_>>(),
            [Some(false), Some(false), None]
        );
        assert_eq!(
            with_trigger_type(&form, TriggerType::DiffPosted).steps,
            form.steps
        );
    }

    #[test]
    fn refuses_invalid_ids_shapes_and_types() {
        for text in [
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: same, action: send_message, text: hi }, { id: same, action: delay, duration: 5m }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: bad-id, action: send_message, text: hi }]\n",
            &format!(
                "name: Test\ntrigger: {{ on: message_posted }}\nsteps: [{{ id: {}, action: send_message, text: hi }}]\n",
                "a".repeat(65)
            ),
            "name: Test\ntrigger: { on: message_posted }\nsteps: { id: s1, action: send_message, text: hi }\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message, text: 42 }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: s1, timeout_secs: 0, action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: s1, timeout_secs: 1.5, action: send_message, text: hi }]\n",
            "name: Test\ndescription: \" spaced \"\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
            "name: Test\ndescription: \"\"\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: reaction_added, emoji: \"\" }\nsteps: [{ id: s1, action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: s1, name: \" spaced \", action: send_message, text: hi }]\n",
            "name: Test\ntrigger: { on: message_posted }\nsteps: [{ id: s1, action: send_message, text: hi, channel: \"\" }]\n",
        ] {
            assert!(yaml_to_form(text).is_err(), "{text}");
        }
        for action in [
            "send_dm",
            "request_approval",
            "add_reaction",
            "set_channel_topic",
        ] {
            let error = yaml_to_form(&format!(
                "name: Advanced\ntrigger: {{on: message_posted}}\nsteps: [{{id: s1, action: {action}}}]\n"
            ))
            .unwrap_err();
            assert!(error.starts_with("Unsupported action") && error.ends_with("YAML editor"));
        }
    }

    #[test]
    fn keeps_conditions_timeouts_and_advanced_filters() {
        let form = accepted(
            "name: C\ntrigger: { on: message_posted }\nsteps: [{ id: s1, if: trigger_author == \"abc\", action: send_message, text: hi }]\n",
        );
        assert_eq!(
            accepted(&form_to_yaml(&form)).steps[0].condition,
            form.steps[0].condition
        );
        let mut timed = accepted(
            "name: Timed\ntrigger: { on: message_posted }\nsteps: [{ id: s1, timeout_secs: 3602, action: send_message, text: hi }]\n",
        );
        assert_eq!(timed.steps[0].timeout_secs.as_deref(), Some("1h 2s"));
        timed.steps[0].timeout_secs = Some("5m".into());
        assert_eq!(
            yaml::parse(&form_to_yaml(&timed)).unwrap()["steps"][0]["timeout_secs"],
            Value::Number(300.into())
        );
        let filter = "str_contains(trigger_text, \"deploy\") && trigger_author == \"abc\"";
        let mut advanced = accepted(&format!(
            "name: Advanced\ndescription: Before\ntrigger:\n  on: message_posted\n  filter: '{filter}'\nsteps:\n  - id: s1\n    action: send_message\n    text: hi\n"
        ));
        advanced.description = "After".into();
        let generated = yaml::parse(&form_to_yaml(&advanced)).unwrap();
        assert_eq!(generated["description"].as_str(), Some("After"));
        assert_eq!(generated["trigger"]["filter"].as_str(), Some(filter));
    }

    #[test]
    fn invalid_timeout_text_survives_for_validation() {
        let with_timeout = |timeout: Option<&str>| {
            let form = WorkflowForm {
                name: "Auto Reply".into(),
                steps: vec![Step {
                    text: Some("hi".into()),
                    timeout_secs: timeout.map(str::to_owned),
                    ..Step::new("step_1".into(), ActionType::SendMessage)
                }],
                ..Default::default()
            };
            yaml::parse(&form_to_yaml(&form)).unwrap()["steps"][0]
                .get("timeout_secs")
                .cloned()
        };
        for timeout in ["oops", "0s", "1.5", "9007199254740992"] {
            assert_eq!(
                with_timeout(Some(timeout)),
                Some(Value::String(timeout.into()))
            );
        }
        for timeout in [None, Some(""), Some(" ")] {
            assert_eq!(with_timeout(timeout), None);
        }
    }

    #[test]
    fn webhook_steps_keep_method_absence_and_header_order() {
        let text = "name: Request\ntrigger:\n  on: webhook\nsteps:\n  - id: request\n    action: call_webhook\n    url: https://example.com/hook\n    headers:\n      Z-First: one\n      A-Second: two\n    body: '{\"ready\":true}'\n";
        let form = accepted(text);
        assert_eq!(form.steps[0].method, None);
        let output = form_to_yaml(&form);
        assert!(!output.contains("method:"));
        assert!(output.find("Z-First") < output.find("A-Second"));
        assert_eq!(
            accepted(&output).steps[0].body.as_deref(),
            Some("{\"ready\":true}")
        );
        for extra in ["    future: keep\n", "    headers:\n      X-Count: 5\n"] {
            assert!(yaml_to_form(&format!("name: Request\ntrigger:\n  on: webhook\nsteps:\n  - id: request\n    action: call_webhook\n    url: https://example.com/hook\n{extra}")).is_err());
        }
        let conditional = accepted(
            "name: Conditional\ntrigger: {on: webhook}\nsteps:\n  - id: request\n    action: call_webhook\n    url: \"{{trigger_text}}\"\n    if: ''\n",
        );
        let output = yaml::parse(&form_to_yaml(&WorkflowForm {
            name: "Renamed".into(),
            ..conditional
        }))
        .unwrap();
        assert_eq!(output["steps"][0]["if"].as_str(), Some(""));
        assert_eq!(output["steps"][0]["url"].as_str(), Some("{{trigger_text}}"));
    }

    #[test]
    fn step_ids_fill_the_first_gap() {
        let steps: Vec<Step> = ["step_9007199254740992", "step_1", "step_3"]
            .into_iter()
            .map(|id| Step::new(id.into(), ActionType::Delay))
            .collect();
        assert_eq!(next_step_id(&steps), "step_2");
        assert_eq!(next_step_id(&[]), "step_1");
    }

    #[test]
    fn new_drafts_start_disabled() {
        assert_eq!(
            new_draft_yaml(),
            "name: Untitled workflow\ntrigger:\n  on: message_posted\nsteps: []\nenabled: false\n"
        );
        assert!(!accepted(&new_draft_yaml()).enabled);
    }
}
