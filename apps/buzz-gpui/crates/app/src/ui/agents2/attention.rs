//! The app's editor for an agent's `agent-attention/v1` objects (buzz-app's
//! `AttentionPanel.tsx`). Every agent type is woken through them, so the
//! app edits them once instead of each type. Watches and timers sit under
//! the Interest they serve; raw JSON covers what the forms leave out (tags,
//! classifiers). Validation is the spec's, at save.

use super::{compact, problem};
use crate::agents2::attention::{
    self as format, AttentionObject, AttentionValue, ChannelChoice, EventWatch, TimerState,
    TimerWatch, UNITS, attention_of, compile_filter, describe_watch, interest_title, parse_slug,
    plural, timer_spent, timer_state, validate_object,
};
use crate::agents2::store::SkippedObject;
use crate::agents2::{Agent, AgentChange, Agents2};
use crate::ui::agents::form::field;
use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::time;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, Checkbox, Colors, Icon, IconButton, IconButtonSize, IconName,
    Menu, MenuItem, MenuSize, Select, Switch, TextArea, TextField, TypeStyled, tokens::radius,
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// What wakes the agent, one line each, for glances like the peek
/// (`AttentionSummary`).
pub fn summary(agent: &Agent, channels: &[ChannelChoice], c: &Colors) -> Div {
    let grouped = attention_of(&agent.attention);
    let on: Vec<&AttentionObject> = grouped
        .watches
        .iter()
        .copied()
        .filter(|object| enabled(&object.value))
        .collect();
    let paused = grouped.watches.len() - on.len();
    let line = |icon: IconName, text: String| {
        div()
            .flex()
            .min_w_0()
            .items_center()
            .gap(rems(0.5))
            .child(Icon::new(icon).size(14.))
            .child(div().truncate().child(text))
    };
    div()
        .flex()
        .flex_col()
        .gap(rems(0.375))
        .text_body_sm()
        .child(line(IconName::At, "Mentions".into()))
        .children(on.iter().map(|object| {
            line(
                watch_icon(&object.value),
                describe_watch(&object.value, channels),
            )
        }))
        .when(paused > 0, |this| {
            this.child(
                div()
                    .text_color(c.text_subtle)
                    .child(plural(paused as u64, "paused watch")),
            )
        })
}

fn enabled(value: &AttentionValue) -> bool {
    match value {
        AttentionValue::Event(watch) => watch.enabled,
        AttentionValue::Timer(timer) => timer.enabled,
        AttentionValue::Interest(_) => true,
    }
}

fn watch_icon(value: &AttentionValue) -> IconName {
    match value {
        AttentionValue::Timer(_) => IconName::Timer,
        _ => IconName::Eye,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A free slug in `space` from `base`. Ids are printable ASCII without
/// spaces, unique within the agent; a skipped object still owns its slug.
pub(crate) fn free_slug(space: &str, base: &str, agent: &Agent) -> String {
    let mut stem = String::new();
    let mut run = false;
    for ch in base.trim().to_lowercase().chars() {
        if ('!'..='~').contains(&ch) {
            stem.push(ch);
            run = false;
        } else if !run {
            stem.push('-');
            run = true;
        }
    }
    let mut stem: String = stem.chars().take(56).collect();
    if stem.is_empty() {
        stem = space.into();
    }
    let taken = |slug: &str| agent.attention.contains_key(slug) || agent.skipped.contains_key(slug);
    let mut id = stem.clone();
    let mut n = 2;
    while taken(&format!("{space}/{id}")) {
        id = format!("{stem}-{n}");
        n += 1;
    }
    format!("{space}/{id}")
}

/// macOS-style curly quotes typed into the filter become straight ones;
/// the grammar only takes `"`.
fn straight_quotes(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
            other => other,
        })
        .collect()
}

/// Where a timer's schedule stands at `at`, for its row: (spent, text).
fn timer_status(timer: &TimerWatch, saved: Option<&TimerState>, at: u64) -> (bool, String) {
    let state = timer_state(timer, saved, at);
    let runs = match timer.max_occurrences {
        // Occurrences missed while no state was saved count as used, so a
        // limited timer can be past its limit; it never runs more than it.
        Some(max) => Some(format!(
            "ran {} of {}",
            state.used.min(max),
            plural(max, "time")
        )),
        None if state.used > 0 => Some(format!("ran {}", plural(state.used, "time"))),
        None => None,
    };
    if timer_spent(timer, &state, at) {
        let ended = timer.expires_at.is_some_and(|end| at >= end);
        return (
            true,
            if ended {
                "Ended".into()
            } else {
                format!("Done, {}", runs.unwrap_or_default())
            },
        );
    }
    let parts: Vec<String> = [
        timer
            .enabled
            .then(|| format!("next {}", time::byline(state.next_due.max(at)))),
        runs,
        timer
            .expires_at
            .map(|end| format!("until {}", time::byline(end))),
    ]
    .into_iter()
    .flatten()
    .collect();
    (false, parts.join(", "))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    /// The Attention switch.
    Switch,
    Row(String),
    NewInterest,
    Interest(String),
    Json(String),
    Draft(String),
}

/// What choosing a select option does.
type Choose = Rc<dyn Fn(&mut AttentionPanel, &mut Context<AttentionPanel>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Picker {
    WakeOn,
    In,
    Replies,
    Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Replies {
    Any,
    Only,
    None,
}

impl Replies {
    fn filter(self) -> &'static str {
        match self {
            Replies::Any => "",
            Replies::Only => "is_reply",
            Replies::None => "!is_reply",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Replies::Any => "Replies or not",
            Replies::Only => "Only replies",
            Replies::None => "Not replies",
        }
    }
}

struct NewInterest {
    name: Entity<InputState>,
    instructions: Entity<TextareaState>,
}

/// Adds one watch or timer for an Interest. Editing an existing one is its
/// JSON, so the form never reads every field of the schema back in.
struct WatchForm {
    timer: bool,
    name: Entity<InputState>,
    /// `None` is every channel it can read.
    scope: Option<Vec<String>>,
    any_kind: bool,
    exactly: Entity<InputState>,
    replies: Replies,
    advanced: Entity<InputState>,
    advanced_open: bool,
    prompt: Entity<TextareaState>,
    count: Entity<InputState>,
    unit: u64,
    limit: Entity<InputState>,
    until: Entity<InputState>,
    picker: Option<Picker>,
    _subscriptions: Vec<Subscription>,
}

pub struct AttentionPanel {
    pubkey: String,
    adding: Option<NewInterest>,
    /// Interests whose instructions are being edited, by slug.
    editing: HashMap<String, Entity<TextareaState>>,
    /// Open watch and timer forms, by Interest id.
    drafts: HashMap<String, WatchForm>,
    /// Open JSON editors, by slug.
    json: HashMap<String, Entity<TextareaState>>,
    pending: HashSet<Key>,
    errors: HashMap<Key, SharedString>,
    /// The open row menu, by slug.
    menu: Option<String>,
    _observe: Option<Subscription>,
}

impl AttentionPanel {
    pub fn new(pubkey: String, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let observe =
            Agents2::global(cx).map(|service| cx.observe(&service, |_, _, cx| cx.notify()));
        Self {
            pubkey,
            adding: None,
            editing: HashMap::new(),
            drafts: HashMap::new(),
            json: HashMap::new(),
            pending: HashSet::new(),
            errors: HashMap::new(),
            menu: None,
            _observe: observe,
        }
    }

    fn agent(&self, cx: &App) -> Option<Agent> {
        Agents2::global(cx)?.read(cx).find(&self.pubkey).cloned()
    }

    /// Saves `changes`, showing progress and errors under `key`; `done`
    /// runs after a successful save.
    fn save(
        &mut self,
        key: Key,
        changes: Vec<(String, Option<Value>)>,
        done: impl FnOnce(&mut Self) + 'static,
        cx: &mut Context<Self>,
    ) {
        let change = AgentChange {
            attention: changes,
            ..Default::default()
        };
        self.save_change(key, change, done, cx);
    }

    fn save_change(
        &mut self,
        key: Key,
        change: AgentChange,
        done: impl FnOnce(&mut Self) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = Agents2::global(cx) else {
            return;
        };
        self.pending.insert(key.clone());
        self.errors.remove(&key);
        let pubkey = self.pubkey.clone();
        let task = service.update(cx, |service, cx| service.save(&pubkey, change, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.pending.remove(&key);
                match result {
                    Ok(()) => done(this),
                    Err(error) => {
                        this.errors.insert(key, error.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn fail(&mut self, key: Key, error: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.errors.insert(key, error.into());
        cx.notify();
    }

    fn dismiss(cx: &mut Context<Self>) -> Dismiss {
        let weak = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.menu = None;
                for draft in this.drafts.values_mut() {
                    draft.picker = None;
                }
                cx.notify();
            });
        })
    }

    // ----- actions -----

    fn start_interest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx));
        let instructions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Keep the release checklist current and flag blockers.")
                .auto_grow(3, 8)
        });
        name.update(cx, |state, cx| state.focus(window, cx));
        self.adding = Some(NewInterest { name, instructions });
        self.errors.remove(&Key::NewInterest);
        cx.notify();
    }

    fn add_interest(&mut self, cx: &mut Context<Self>) {
        let (Some(form), Some(agent)) = (&self.adding, self.agent(cx)) else {
            return;
        };
        let name = form.name.read(cx).value().to_string();
        let instructions = form.instructions.read(cx).value().to_string();
        let slug = free_slug("interest", &name, &agent);
        let value = json!({ "type": "interest", "instructions": instructions });
        let problem = if name.trim().is_empty() {
            Some("Name the interest".to_string())
        } else {
            validate_object(&slug, &value)
        };
        match problem {
            Some(problem) => self.fail(Key::NewInterest, problem, cx),
            None => self.save(
                Key::NewInterest,
                vec![(slug, Some(value))],
                |this| this.adding = None,
                cx,
            ),
        }
    }

    fn edit_interest(
        &mut self,
        slug: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.to_owned();
        let state = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).auto_grow(3, 10);
            state.set_value(text, window, cx);
            state
        });
        self.editing.insert(slug.to_owned(), state);
        self.menu = None;
        cx.notify();
    }

    fn save_interest(&mut self, slug: String, cx: &mut Context<Self>) {
        let Some(state) = self.editing.get(&slug) else {
            return;
        };
        let instructions = state.read(cx).value().to_string();
        let done_slug = slug.clone();
        self.save(
            Key::Interest(slug.clone()),
            vec![(
                slug,
                Some(json!({ "type": "interest", "instructions": instructions })),
            )],
            move |this| {
                this.editing.remove(&done_slug);
            },
            cx,
        );
    }

    fn open_json(&mut self, slug: &str, value: Value, window: &mut Window, cx: &mut Context<Self>) {
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into());
        let state = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).auto_grow(10, 24);
            state.set_value(text, window, cx);
            state
        });
        self.json.insert(slug.to_owned(), state);
        self.errors.remove(&Key::Json(slug.to_owned()));
        self.menu = None;
        cx.notify();
    }

    fn save_json(&mut self, slug: String, cx: &mut Context<Self>) {
        let Some(state) = self.json.get(&slug) else {
            return;
        };
        let key = Key::Json(slug.clone());
        let Ok(value) = serde_json::from_str::<Value>(state.read(cx).value().as_ref()) else {
            return self.fail(key, "Enter a JSON object", cx);
        };
        if let Some(problem) = validate_object(&slug, &value) {
            return self.fail(key, problem, cx);
        }
        let done = slug.clone();
        self.save(
            key,
            vec![(slug, Some(value))],
            move |this| {
                this.json.remove(&done);
            },
            cx,
        );
    }

    fn start_draft(
        &mut self,
        interest: &str,
        timer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("Release questions", window, cx);
        let exactly = input("deploy", window, cx);
        let advanced = input("", window, cx);
        let count = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value("1", window, cx);
            state
        });
        let limit = input("", window, cx);
        let until = input("YYYY-MM-DD HH:MM", window, cx);
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Post a summary of open questions.")
                .auto_grow(2, 8)
        });
        let subscriptions = vec![cx.subscribe_in(
            &advanced,
            window,
            |_, state, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = state.read(cx).value().to_string();
                    let straight = straight_quotes(&text);
                    if straight != text {
                        state.update(cx, |state, cx| state.set_value(straight, window, cx));
                    }
                    cx.notify();
                }
            },
        )];
        if timer {
            prompt.update(cx, |state, cx| state.focus(window, cx));
        } else {
            name.update(cx, |state, cx| state.focus(window, cx));
        }
        self.drafts.insert(
            interest.to_owned(),
            WatchForm {
                timer,
                name,
                scope: None,
                any_kind: false,
                exactly,
                replies: Replies::Any,
                advanced,
                advanced_open: false,
                prompt,
                count,
                unit: format::interval(3_600).2,
                limit,
                until,
                picker: None,
                _subscriptions: subscriptions,
            },
        );
        self.errors.remove(&Key::Draft(interest.to_owned()));
        cx.notify();
    }

    fn add_watch(&mut self, interest: String, cx: &mut Context<Self>) {
        let (Some(form), Some(agent)) = (self.drafts.get(&interest), self.agent(cx)) else {
            return;
        };
        let key = Key::Draft(interest.clone());
        let advanced = form.advanced.read(cx).value().trim().to_string();
        let filter_problem = (!advanced.is_empty())
            .then(|| compile_filter(&advanced).err())
            .flatten();
        let at = now();
        let (slug, value, expires) = if form.timer {
            let count: f64 = form
                .count
                .read(cx)
                .value()
                .trim()
                .parse()
                .unwrap_or(f64::NAN);
            let seconds = (count * form.unit as f64).round();
            let limit = form.limit.read(cx).value().trim().to_string();
            let until = form.until.read(cx).value().trim().to_string();
            let expires = (!until.is_empty()).then(|| parse_local(&until));
            let value = json!({
                "type": "timer",
                "interest_id": interest,
                "prompt": form.prompt.read(cx).value().to_string(),
                "enabled": true,
                "interval_secs": number(seconds),
                "armed_at": at,
                "max_occurrences": if limit.is_empty() {
                    Value::Null
                } else {
                    limit.parse::<f64>().map(number).unwrap_or(Value::String(limit))
                },
                "expires_at": expires.flatten().map_or(Value::Null, Value::from),
            });
            (free_slug("watch", "timer", &agent), value, expires)
        } else {
            let name = form.name.read(cx).value().trim().to_string();
            let exactly = form.exactly.read(cx).value().to_string();
            let replies = form.replies.filter();
            // The simple fields and the advanced expression, all of which
            // must hold.
            let mut parts: Vec<String> = Vec::new();
            if !exactly.is_empty() {
                parts.push(format!(
                    "content == {}",
                    serde_json::to_string(&exactly).expect("strings serialize")
                ));
            }
            if !replies.is_empty() {
                parts.push(replies.into());
            }
            if !advanced.is_empty() {
                parts.push(if !exactly.is_empty() || !replies.is_empty() {
                    format!("({advanced})")
                } else {
                    advanced.clone()
                });
            }
            let filter = parts.join(" && ");
            let mut value = json!({
                "type": "event",
                "interest_id": interest,
                "enabled": true,
                "since": at,
                "channels": match &form.scope {
                    None => json!("all"),
                    Some(ids) => json!(ids),
                },
                "kinds": if form.any_kind { json!([]) } else { json!([9]) },
            });
            if !name.is_empty() {
                value["name"] = json!(name);
            }
            if !filter.is_empty() {
                value["filter"] = json!(filter);
            }
            let base = if name.is_empty() {
                "messages"
            } else {
                name.as_str()
            };
            (free_slug("watch", base, &agent), value, None)
        };
        let problem = if filter_problem.is_some() {
            // Already shown under the field.
            return cx.notify();
        } else if form.scope.as_ref().is_some_and(Vec::is_empty) {
            Some("Choose at least one channel".to_string())
        } else if matches!(expires, Some(None)) || expires.flatten().is_some_and(|end| end <= at) {
            Some("Choose an end time in the future".to_string())
        } else {
            validate_object(&slug, &value)
        };
        match problem {
            Some(problem) => self.fail(key, problem, cx),
            None => {
                let done = interest.clone();
                self.save(
                    key,
                    vec![(slug, Some(value))],
                    move |this| {
                        this.drafts.remove(&done);
                    },
                    cx,
                )
            }
        }
    }

    fn put(&mut self, slug: String, value: Option<Value>, cx: &mut Context<Self>) {
        self.menu = None;
        self.save(Key::Row(slug.clone()), vec![(slug, value)], |_| {}, cx);
    }

    /// The owner's switch for the agent's own attention.
    fn attention_switch(&self, agent: &Agent, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let weak = cx.entity().downgrade();
        let switch = Switch::new("attention-on", agent.attention_enabled)
            .disabled(self.pending.contains(&Key::Switch))
            .on_change(move |checked, _, cx| {
                let change = AgentChange {
                    attention_enabled: Some(checked),
                    ..Default::default()
                };
                let _ = weak.update(cx, |this, cx| {
                    this.save_change(Key::Switch, change, |_| {}, cx)
                });
            });
        preference_row(
            IconName::Eye,
            "Attention",
            Some(
                div()
                    .child(
                        "Lets the agent add and change its own Interests, watches and timers, \
                         and wakes it for them. Off, nothing below wakes it, and it is not \
                         offered the tools.",
                    )
                    .children(problem(self.errors.get(&Key::Switch), &c))
                    .into_any_element(),
            ),
            false,
            Some(switch.into_any_element()),
            &c,
        )
    }
}

/// A JSON number, or `null` when it is not finite, which validation
/// refuses with the field's message.
fn number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value >= 0.0 {
        Value::from(value as u64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// `YYYY-MM-DD HH:MM` (or with a `T`) in local time, as Unix seconds.
fn parse_local(text: &str) -> Option<u64> {
    use chrono::{Local, NaiveDateTime, TimeZone};
    let text = text.replace('T', " ");
    let naive = NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    let local = Local.from_local_datetime(&naive).earliest()?;
    u64::try_from(local.timestamp()).ok()
}

// ----- rendering -----

/// `SettingsGroup`: an outlined card with a divider between rows.
fn group(rows: Vec<AnyElement>, c: &Colors) -> Div {
    let count = rows.len();
    let border = c.border_standard;
    div()
        .flex()
        .flex_col()
        .px(rems(1.))
        .border_1()
        .border_color(border)
        .rounded(radius::CONTAINER)
        .children(rows.into_iter().enumerate().map(move |(index, row)| {
            div()
                .when(index + 1 < count, |this| {
                    this.border_b_1().border_color(border)
                })
                .child(row)
        }))
}

/// `PreferenceRow`: icon, title and subtitle, trailing controls.
fn preference_row(
    icon: IconName,
    title: impl Into<SharedString>,
    subtitle: Option<AnyElement>,
    disabled: bool,
    trailing: Option<AnyElement>,
    c: &Colors,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(rems(1.))
        .py(rems(0.75))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(rems(1.25))
                .text_color(c.text_subtle)
                .child(Icon::new(icon).size(16.)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(rems(0.125))
                .when(disabled, |this| this.text_color(c.text_unavailable))
                .child(div().text_label_sm().child(title.into()))
                .children(subtitle.map(|subtitle| {
                    div()
                        .text_caption()
                        .text_color(if disabled {
                            c.text_unavailable
                        } else {
                            c.text_subtle
                        })
                        .child(subtitle)
                })),
        )
        .children(trailing.map(|trailing| div().flex().flex_none().items_center().child(trailing)))
}

fn section_heading(
    title: impl Into<SharedString>,
    description: Option<&'static str>,
    c: &Colors,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rems(0.125))
        .child(div().text_label().child(title.into()))
        .children(
            description.map(|text| div().text_body_sm().text_color(c.text_subtle).child(text)),
        )
}

/// The inset card new forms sit in.
fn inset_card(c: &Colors) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rems(0.75))
        .p(rems(1.))
        .rounded(radius::CONTAINER)
        .bg(c.surface_inset)
}

struct MenuSpec {
    id: String,
    label: String,
    edit_label: &'static str,
    remove_label: &'static str,
    remove_disabled: bool,
}

impl AttentionPanel {
    fn form_actions(
        &self,
        id: &str,
        key: Key,
        submit: &'static str,
        on_submit: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        on_cancel: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        let pending = self.pending.contains(&key);
        div()
            .flex()
            .gap(rems(0.5))
            .child(
                Button::new(SharedString::from(format!("{id}-submit")), submit)
                    .size(ButtonSize::Sm)
                    .prominent()
                    .loading(pending)
                    .on_click(cx.listener(move |this, _, window, cx| on_submit(this, window, cx))),
            )
            .child(
                Button::new(SharedString::from(format!("{id}-cancel")), "Cancel")
                    .size(ButtonSize::Sm)
                    .disabled(pending)
                    .on_click(cx.listener(move |this, _, window, cx| on_cancel(this, window, cx))),
            )
    }

    /// The row's "Actions for …" menu: Edit, any extra item, Remove.
    fn row_menu(
        &self,
        spec: MenuSpec,
        on_edit: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        extra: Option<MenuItem>,
        on_remove: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.menu.as_deref() == Some(spec.id.as_str());
        let id = spec.id.clone();
        div()
            .relative()
            .child(
                IconButton::new(
                    SharedString::from(format!("menu-{}", spec.id)),
                    IconName::DotsThree,
                )
                .size(IconButtonSize::Sm)
                .icon_size(18.)
                .selected(open)
                .when(!open, |this| {
                    this.tooltip(format!("Actions for {}", spec.label))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = (this.menu.as_deref() != Some(id.as_str())).then(|| id.clone());
                    cx.notify();
                })),
            )
            .when(open, |this| {
                let mut menu = Menu::new().size(MenuSize::Compact).child(
                    MenuItem::new("edit", spec.edit_label)
                        .icon(IconName::PencilSimple)
                        .icon_size(14.)
                        .on_click(
                            cx.listener(move |this, _, window, cx| on_edit(this, window, cx)),
                        ),
                );
                if let Some(extra) = extra {
                    menu = menu.child(extra);
                }
                menu = menu.child(
                    MenuItem::new("remove", spec.remove_label)
                        .icon(IconName::Trash)
                        .icon_size(14.)
                        .danger()
                        .disabled(spec.remove_disabled)
                        .on_click(
                            cx.listener(move |this, _, window, cx| on_remove(this, window, cx)),
                        ),
                );
                this.child(overlay::dropdown(
                    window,
                    Align::End,
                    menu,
                    Self::dismiss(cx),
                ))
            })
    }

    fn json_editor(&self, slug: &str, cx: &mut Context<Self>) -> Option<Div> {
        let state = self.json.get(slug)?;
        let c = *cx.colors();
        let key = Key::Json(slug.to_owned());
        let save_slug = slug.to_owned();
        let cancel_slug = slug.to_owned();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.5))
                .p(rems(0.75))
                .child(field(
                    slug.to_owned(),
                    Some(format::ATTENTION_SCHEMA.into()),
                    TextArea::new(state).rows(10).code(),
                    &c,
                ))
                .children(problem(self.errors.get(&key), &c))
                .child(self.form_actions(
                    &format!("json-{slug}"),
                    key,
                    "Save",
                    move |this, _, cx| this.save_json(save_slug.clone(), cx),
                    move |this, _, cx| {
                        this.json.remove(&cancel_slug);
                        cx.notify();
                    },
                    cx,
                )),
        )
    }

    fn watch_row(
        &self,
        object: &AttentionObject,
        agent: &Agent,
        channels: &[ChannelChoice],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let slug = object.slug.clone();
        let value = object.value.clone();
        let key = Key::Row(slug.clone());
        let pending = self.pending.contains(&key);
        let title = describe_watch(&value, channels);
        let (detail, spent) = match &value {
            AttentionValue::Timer(timer) => {
                let (spent, status) = timer_status(timer, agent.timers.get(&slug), now());
                (
                    vec![
                        Some(format!("\u{201c}{}\u{201d}", timer.prompt)),
                        Some(status).filter(|s| !s.is_empty()),
                    ],
                    spent,
                )
            }
            AttentionValue::Event(watch) => (
                vec![
                    watch.name.as_ref().map(|_| {
                        describe_watch(
                            &AttentionValue::Event(EventWatch {
                                name: None,
                                ..watch.clone()
                            }),
                            channels,
                        )
                    }),
                    watch.filter.as_ref().map(|filter| format!("when {filter}")),
                    watch.classifier.as_ref().map(|_| {
                        "has a classifier, which this app cannot run yet, so it matches without it"
                            .to_string()
                    }),
                ],
                false,
            ),
            AttentionValue::Interest(_) => (Vec::new(), false),
        };
        let detail: Vec<String> = detail.into_iter().flatten().collect();
        let error = self.errors.get(&key).cloned();
        let subtitle = (!detail.is_empty() || error.is_some()).then(|| {
            div()
                .flex()
                .flex_col()
                .child(detail.join(" · "))
                .children(problem(error.as_ref(), &c))
                .into_any_element()
        });
        let on = enabled(&value);
        let toggle_slug = slug.clone();
        let toggle_value = value.clone();
        let weak = cx.entity().downgrade();
        let switch = Switch::new(SharedString::from(format!("switch-{slug}")), on)
            .disabled(pending)
            .on_change(move |checked, _, cx| {
                let mut json = toggle_value.to_json();
                json["enabled"] = json!(checked);
                let _ = weak.update(cx, |this, cx| this.put(toggle_slug.clone(), Some(json), cx));
            });
        let edit_slug = slug.clone();
        let edit_value = value.to_json();
        let remove_slug = slug.clone();
        let again = match (&value, spent) {
            (AttentionValue::Timer(timer), true) => {
                // The owner starts it over: a new schedule, nothing used yet.
                let at = now();
                let mut json = AttentionValue::Timer(timer.clone()).to_json();
                json["enabled"] = json!(true);
                json["armed_at"] = json!(at);
                if timer.expires_at.is_some_and(|end| end <= at) {
                    json["expires_at"] = Value::Null;
                }
                let again_slug = slug.clone();
                Some(
                    MenuItem::new("again", "Run again")
                        .icon(IconName::Timer)
                        .icon_size(14.)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            let change = AgentChange {
                                attention: vec![(again_slug.clone(), Some(json.clone()))],
                                restart: vec![again_slug.clone()],
                                ..Default::default()
                            };
                            this.save_change(Key::Row(again_slug.clone()), change, |_| {}, cx)
                        })),
                )
            }
            _ => None,
        };
        let menu = self.row_menu(
            MenuSpec {
                id: slug.clone(),
                label: title.clone(),
                edit_label: "Edit JSON",
                remove_label: "Remove",
                remove_disabled: false,
            },
            move |this, window, cx| this.open_json(&edit_slug, edit_value.clone(), window, cx),
            again,
            move |this, _, cx| this.put(remove_slug.clone(), None, cx),
            window,
            cx,
        );
        div()
            .flex()
            .flex_col()
            .child(preference_row(
                watch_icon(&value),
                title,
                subtitle,
                !on,
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(rems(0.25))
                        .child(switch)
                        .child(menu)
                        .into_any_element(),
                ),
                &c,
            ))
            .children(self.json_editor(&slug, cx))
            .into_any_element()
    }

    fn skipped_row(
        &self,
        object: &SkippedObject,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let slug = object.slug.clone();
        let key = Key::Row(slug.clone());
        let edit_slug = slug.clone();
        let edit_value = object.value.clone();
        let remove_slug = slug.clone();
        let subtitle = div()
            .flex()
            .flex_col()
            .child(object.problem.clone())
            .children(problem(self.errors.get(&key), &c))
            .into_any_element();
        let menu = self.row_menu(
            MenuSpec {
                id: slug.clone(),
                label: slug.clone(),
                edit_label: "Edit JSON",
                remove_label: "Remove",
                remove_disabled: false,
            },
            move |this, window, cx| this.open_json(&edit_slug, edit_value.clone(), window, cx),
            None,
            move |this, _, cx| this.put(remove_slug.clone(), None, cx),
            window,
            cx,
        );
        div()
            .flex()
            .flex_col()
            .child(preference_row(
                IconName::WarningCircle,
                slug.clone(),
                Some(subtitle),
                false,
                Some(menu.into_any_element()),
                &c,
            ))
            .children(self.json_editor(&slug, cx))
            .into_any_element()
    }

    fn interest_group(
        &self,
        object: &AttentionObject,
        agent: &Agent,
        channels: &[ChannelChoice],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = *cx.colors();
        let slug = object.slug.clone();
        let id = parse_slug(&slug)
            .map(|(_, id)| id.to_owned())
            .unwrap_or_else(|| slug.clone());
        let instructions = match &object.value {
            AttentionValue::Interest(interest) => interest.instructions.clone(),
            _ => String::new(),
        };
        let title = interest_title(&id);
        let key = Key::Interest(slug.clone());
        let watches: Vec<&AttentionObject> = agent
            .attention
            .values()
            .filter(|object| object.value.interest_id() == Some(id.as_str()))
            .collect();
        let edit_slug = slug.clone();
        let edit_text = instructions.clone();
        let remove_slug = slug.clone();
        // The spec keeps an Interest while anything still uses it.
        let menu = self.row_menu(
            MenuSpec {
                id: slug.clone(),
                label: title.clone(),
                edit_label: "Edit",
                remove_label: if watches.is_empty() {
                    "Remove"
                } else {
                    "Remove (first remove what uses it)"
                },
                remove_disabled: !watches.is_empty(),
            },
            move |this, window, cx| this.edit_interest(&edit_slug, &edit_text, window, cx),
            None,
            move |this, _, cx| {
                this.menu = None;
                let slug = remove_slug.clone();
                this.save(Key::Interest(slug.clone()), vec![(slug, None)], |_| {}, cx);
            },
            window,
            cx,
        );
        let body = match self.editing.get(&slug) {
            Some(state) => {
                let save_slug = slug.clone();
                let cancel_slug = slug.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(TextArea::new(state).rows(3))
                    .child(self.form_actions(
                        &format!("interest-{id}"),
                        key.clone(),
                        "Save",
                        move |this, _, cx| this.save_interest(save_slug.clone(), cx),
                        move |this, _, cx| {
                            this.editing.remove(&cancel_slug);
                            cx.notify();
                        },
                        cx,
                    ))
            }
            None => div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .whitespace_normal()
                .child(instructions.clone()),
        };
        let draft = self.drafts.get(&id);
        let rows: Vec<AnyElement> = watches
            .iter()
            .map(|object| self.watch_row(object, agent, channels, window, cx))
            .collect();
        let watch_id = id.clone();
        let timer_id = id.clone();
        div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(rems(0.5))
                    .child(div().text_label().child(title.clone()))
                    .child(menu),
            )
            .child(body)
            .children(problem(self.errors.get(&key), &c))
            .map(|this| {
                if !rows.is_empty() {
                    this.child(group(rows, &c))
                } else if draft.is_none() {
                    this.child(div().text_body_sm().text_color(c.text_subtle).child(
                        "Nothing wakes it for this yet. Add a watch to wake it on events, or a \
                         timer to wake it on a schedule.",
                    ))
                } else {
                    this
                }
            })
            .map(|this| match draft {
                Some(form) => this.child(self.watch_form(&id, form, channels, window, cx)),
                None => this.child(
                    div()
                        .flex()
                        .gap(rems(0.5))
                        .child(
                            compact(SharedString::from(format!("add-watch-{id}")), "Add watch")
                                .icon(IconName::Eye)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_draft(&watch_id, false, window, cx)
                                })),
                        )
                        .child(
                            compact(SharedString::from(format!("add-timer-{id}")), "Add timer")
                                .icon(IconName::Timer)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_draft(&timer_id, true, window, cx)
                                })),
                        ),
                ),
            })
    }

    fn select(
        &self,
        interest: &str,
        picker: Picker,
        value: String,
        options: Vec<(String, bool, Choose)>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.drafts.get(interest).and_then(|form| form.picker) == Some(picker);
        let toggle = interest.to_owned();
        let mut menu = Menu::new();
        for (index, (label, checked, choose)) in options.into_iter().enumerate() {
            let pick = interest.to_owned();
            menu = menu.child(
                MenuItem::new(("option", index), label)
                    .checked(checked)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        choose(this, cx);
                        if let Some(form) = this.drafts.get_mut(&pick) {
                            form.picker = None;
                        }
                        cx.notify();
                    })),
            );
        }
        div()
            .relative()
            .w_full()
            .child(
                Select::new(SharedString::from(format!("select-{interest}-{picker:?}")))
                    .value(value)
                    .open(open)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = this.drafts.get_mut(&toggle) {
                            form.picker = (form.picker != Some(picker)).then_some(picker);
                        }
                        cx.notify();
                    })),
            )
            .when(open, |this| {
                this.child(overlay::dropdown(
                    window,
                    Align::Start,
                    div().min_w(rems(16.)).child(menu),
                    Self::dismiss(cx),
                ))
            })
    }

    fn watch_form(
        &self,
        interest: &str,
        form: &WatchForm,
        channels: &[ChannelChoice],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = *cx.colors();
        let key = Key::Draft(interest.to_owned());
        let on = |interest: &str, f: fn(&mut WatchForm)| -> Choose {
            let interest = interest.to_owned();
            Rc::new(
                move |this: &mut AttentionPanel, _: &mut Context<AttentionPanel>| {
                    if let Some(form) = this.drafts.get_mut(&interest) {
                        f(form);
                    }
                },
            )
        };
        let mut card = inset_card(&c);
        if !form.timer {
            let advanced = form.advanced.read(cx).value().trim().to_string();
            let filter_problem = (!advanced.is_empty())
                .then(|| compile_filter(&advanced).err())
                .flatten();
            card = card
                .child(field(
                    "Name (optional)",
                    None,
                    TextField::new(&form.name),
                    &c,
                ))
                .child(field(
                    "Wake on",
                    None,
                    self.select(
                        interest,
                        Picker::WakeOn,
                        if form.any_kind { "Any event" } else { "Messages" }.into(),
                        vec![
                            ("Messages".into(), !form.any_kind, on(interest, |f| f.any_kind = false)),
                            ("Any event".into(), form.any_kind, on(interest, |f| f.any_kind = true)),
                        ],
                        window,
                        cx,
                    ),
                    &c,
                ))
                .child(field(
                    "In",
                    None,
                    self.select(
                        interest,
                        Picker::In,
                        if form.scope.is_none() {
                            "Every channel it can read"
                        } else {
                            "Chosen channels"
                        }
                        .into(),
                        vec![
                            (
                                "Every channel it can read".into(),
                                form.scope.is_none(),
                                on(interest, |f| f.scope = None),
                            ),
                            (
                                "Chosen channels".into(),
                                form.scope.is_some(),
                                on(interest, |f| {
                                    if f.scope.is_none() {
                                        f.scope = Some(Vec::new())
                                    }
                                }),
                            ),
                        ],
                        window,
                        cx,
                    ),
                    &c,
                ))
                .when_some(form.scope.clone(), |this, scope| {
                    let boxes: Vec<AnyElement> = channels
                        .iter()
                        .map(|channel| {
                            let id = channel.id.clone();
                            let interest = interest.to_owned();
                            Checkbox::new(
                                SharedString::from(format!("channel-{interest}-{id}")),
                                scope.contains(&channel.id),
                            )
                            .label(format!("#{}", channel.name))
                            .on_change({
                                let weak = cx.entity().downgrade();
                                move |checked, _, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        if let Some(Some(scope)) =
                                            this.drafts.get_mut(&interest).map(|f| f.scope.as_mut())
                                        {
                                            scope.retain(|existing| *existing != id);
                                            if checked {
                                                scope.push(id.clone());
                                            }
                                        }
                                        cx.notify();
                                    });
                                }
                            })
                            .into_any_element()
                        })
                        .collect();
                    this.child(
                        div()
                            .id(SharedString::from(format!("channels-{interest}")))
                            .flex()
                            .flex_col()
                            .gap(rems(0.25))
                            .max_h(rems(12.))
                            .overflow_y_scroll()
                            .children(boxes)
                            .when(channels.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_body_sm()
                                        .text_color(c.text_subtle)
                                        .child("No channels to choose from yet."),
                                )
                            }),
                    )
                })
                .child(field(
                    "Text is exactly (optional)",
                    Some("The whole message, not a part of it".into()),
                    TextField::new(&form.exactly),
                    &c,
                ))
                .child(field(
                    "Replies",
                    None,
                    self.select(
                        interest,
                        Picker::Replies,
                        form.replies.label().into(),
                        [Replies::Any, Replies::Only, Replies::None]
                            .into_iter()
                            .map(|choice| {
                                let interest = interest.to_owned();
                                let choose: Choose = Rc::new(move |this: &mut AttentionPanel, _| {
                                    if let Some(form) = this.drafts.get_mut(&interest) {
                                        form.replies = choice;
                                    }
                                });
                                (choice.label().to_string(), form.replies == choice, choose)
                            })
                            .collect(),
                        window,
                        cx,
                    ),
                    &c,
                ))
                .child({
                    let toggle = interest.to_owned();
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.5))
                        .child(
                            div()
                                .id(SharedString::from(format!("advanced-{interest}")))
                                .flex()
                                .items_center()
                                .gap(rems(0.25))
                                .cursor_pointer()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child(
                                    Icon::new(if form.advanced_open {
                                        IconName::CaretDown
                                    } else {
                                        IconName::CaretRight
                                    })
                                    .size(14.),
                                )
                                .child("Advanced filter")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(form) = this.drafts.get_mut(&toggle) {
                                        form.advanced_open = !form.advanced_open;
                                    }
                                    cx.notify();
                                })),
                        )
                        .when(form.advanced_open, |this| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(rems(0.5))
                                    .child(div().text_label_sm().child("Filter expression (optional)"))
                                    .child(
                                        TextField::new(&form.advanced)

                                            .invalid(filter_problem.is_some()),
                                    )
                                    .child(
                                        div()
                                            .text_body_sm()
                                            .text_color(if filter_problem.is_some() {
                                                c.text_danger
                                            } else {
                                                c.text_subtle
                                            })
                                            .child(filter_problem.clone().unwrap_or_else(|| {
                                                "e.g. author == \"<hex key>\" || content == \"help\". Use &&, ||, ! and ( )."
                                                    .into()
                                            })),
                                    ),
                            )
                        })
                });
        } else {
            let units: Vec<(String, bool, Choose)> = UNITS[..3]
                .iter()
                .map(|(label, size)| {
                    let interest = interest.to_owned();
                    let size = *size;
                    let choose: Choose = Rc::new(move |this: &mut AttentionPanel, _| {
                        if let Some(form) = this.drafts.get_mut(&interest) {
                            form.unit = size;
                        }
                    });
                    (format!("{label}s"), form.unit == size, choose)
                })
                .collect();
            let unit_label = UNITS[..3]
                .iter()
                .find(|(_, size)| *size == form.unit)
                .map(|(label, _)| format!("{label}s"))
                .unwrap_or_default();
            card = card
                .child(field(
                    "Prompt",
                    None,
                    TextArea::new(&form.prompt).rows(2),
                    &c,
                ))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(rems(0.5))
                        .child(div().w(rems(6.)).flex_none().child(field(
                            "Every",
                            None,
                            TextField::new(&form.count),
                            &c,
                        )))
                        .child(div().flex_1().child(self.select(
                            interest,
                            Picker::Unit,
                            unit_label,
                            units,
                            window,
                            cx,
                        ))),
                )
                .child(field(
                    "Stop after (optional)",
                    Some("Times it runs".into()),
                    TextField::new(&form.limit),
                    &c,
                ))
                .child(field(
                    "Until (optional)",
                    Some("It stops at this time".into()),
                    TextField::new(&form.until),
                    &c,
                ));
        }
        let submit_interest = interest.to_owned();
        let cancel_interest = interest.to_owned();
        card.children(problem(self.errors.get(&key), &c))
            .child(self.form_actions(
                &format!("draft-{interest}"),
                key,
                if form.timer { "Add timer" } else { "Add watch" },
                move |this, _, cx| this.add_watch(submit_interest.clone(), cx),
                move |this, _, cx| {
                    this.drafts.remove(&cancel_interest);
                    cx.notify();
                },
                cx,
            ))
    }
}

impl Render for AttentionPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let Some(agent) = self.agent(cx) else {
            return div();
        };
        let channels = Agents2::global(cx)
            .map(|service| service.read(cx).channel_choices(cx))
            .unwrap_or_default();
        let grouped = attention_of(&agent.attention);
        let interests: Vec<AttentionObject> = grouped.interests.into_iter().cloned().collect();
        let orphans: Vec<AttentionObject> = grouped.orphans.into_iter().cloned().collect();
        let skipped: Vec<SkippedObject> = agent.skipped.values().cloned().collect();

        let mut column = div()
            .flex()
            .flex_col()
            .gap(rems(1.5))
            .min_w_0()
            .child(div().text_body_sm().text_color(c.text_subtle).child(
                "What wakes this agent. Mentions always do. For anything else, including \
                 replies in its threads that don't mention it, add an Interest: something it \
                 looks after, with instructions. The watches and timers under an Interest \
                 decide when it wakes for it.",
            ))
            .child(group(
                vec![
                    preference_row(
                        IconName::At,
                        "Mentions",
                        Some(
                            div()
                                .child("Messages that mention it. Always on.")
                                .into_any_element(),
                        ),
                        false,
                        None,
                        &c,
                    )
                    .into_any_element(),
                    self.attention_switch(&agent, cx).into_any_element(),
                ],
                &c,
            ));
        for object in &interests {
            column = column.child(self.interest_group(object, &agent, &channels, window, cx));
        }
        if !orphans.is_empty() {
            let rows = orphans
                .iter()
                .map(|object| self.watch_row(object, &agent, &channels, window, cx))
                .collect();
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(section_heading(
                        "Without an interest",
                        Some(
                            "These name an Interest that does not exist, so they wake the agent \
                             without instructions. Edit the JSON to set its interest_id, or \
                             remove it.",
                        ),
                        &c,
                    ))
                    .child(group(rows, &c)),
            );
        }
        if !skipped.is_empty() {
            let rows = skipped
                .iter()
                .map(|object| self.skipped_row(object, window, cx))
                .collect();
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(section_heading(
                        "Skipped",
                        Some(
                            "These are saved but never wake the agent, because they are invalid \
                             or over a limit. Fix the JSON, or remove them.",
                        ),
                        &c,
                    ))
                    .child(group(rows, &c)),
            );
        }
        column = match &self.adding {
            Some(form) => column.child(
                inset_card(&c)
                    .child(field(
                        "Interest",
                        Some("Something it looks after, e.g. Releases".into()),
                        TextField::new(&form.name),
                        &c,
                    ))
                    .child(field(
                        "Instructions",
                        None,
                        TextArea::new(&form.instructions).rows(3),
                        &c,
                    ))
                    .children(problem(self.errors.get(&Key::NewInterest), &c))
                    .child(self.form_actions(
                        "new-interest",
                        Key::NewInterest,
                        "Add interest",
                        |this, _, cx| this.add_interest(cx),
                        |this, _, cx| {
                            this.adding = None;
                            cx.notify();
                        },
                        cx,
                    )),
            ),
            None => column.child(
                div().flex().child(
                    compact("add-interest", "Add interest")
                        .icon(IconName::Plus)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_interest(window, cx)),
                        ),
                ),
            ),
        };
        column
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn agent(slugs: &[&str]) -> Agent {
        let mut attention = BTreeMap::new();
        for slug in slugs {
            attention.insert(
                slug.to_string(),
                AttentionObject {
                    slug: slug.to_string(),
                    value: AttentionValue::Interest(format::Interest {
                        instructions: "x".into(),
                    }),
                    modified_at: 1,
                },
            );
        }
        Agent {
            pubkey: "a".repeat(64),
            name: "Ada".into(),
            agent_type: "p/t".into(),
            owner: "b".repeat(64),
            relay: "wss://relay.example".into(),
            picture: None,
            attention: Arc::new(attention),
            skipped: Arc::default(),
            timers: Arc::default(),
            attention_enabled: true,
            config: Value::Null,
        }
    }

    #[test]
    fn slugs_are_free_printable_ascii() {
        let none = agent(&[]);
        assert_eq!(
            free_slug("interest", "Release Triage", &none),
            "interest/release-triage"
        );
        assert_eq!(free_slug("interest", "  ", &none), "interest/interest");
        assert_eq!(free_slug("watch", "Café  notes", &none), "watch/caf-notes");
        let taken = agent(&["interest/releases", "interest/releases-2"]);
        assert_eq!(
            free_slug("interest", "Releases", &taken),
            "interest/releases-3"
        );
        assert_eq!(
            free_slug("interest", &"x".repeat(80), &none).len(),
            "interest/".len() + 56
        );
    }

    fn timer(max: Option<u64>) -> TimerWatch {
        TimerWatch {
            interest_id: "default".into(),
            prompt: "tick".into(),
            enabled: true,
            interval_secs: 60,
            armed_at: 1_000,
            max_occurrences: max,
            expires_at: None,
        }
    }

    fn state(used: u64) -> TimerState {
        TimerState {
            armed_at: 1_000,
            next_due: 1_000 + (used + 1) * 60,
            used,
        }
    }

    #[test]
    fn timer_rows_never_count_past_the_limit() {
        // Saved state lost after ten minutes: what was missed counts as
        // used, so the timer is done without running, and says so within
        // its limit.
        let (spent, text) = timer_status(&timer(Some(1)), None, 1_600);
        assert!(spent);
        assert_eq!(text, "Done, ran 1 of 1 time");
        let (spent, text) = timer_status(&timer(Some(1)), Some(&state(11)), 1_700);
        assert!(spent);
        assert_eq!(text, "Done, ran 1 of 1 time");
        let (spent, text) = timer_status(&timer(Some(5)), Some(&state(2)), 1_130);
        assert!(!spent);
        assert!(text.ends_with(", ran 2 of 5 times"), "{text}");
        assert!(
            timer_status(&timer(None), Some(&state(1)), 1_100)
                .1
                .ends_with(", ran 1 time")
        );
        assert!(
            timer_status(&timer(None), Some(&state(3)), 1_230)
                .1
                .ends_with(", ran 3 times")
        );
        assert!(
            !timer_status(&timer(None), Some(&state(0)), 1_000)
                .1
                .contains("ran")
        );
        let ended = TimerWatch {
            expires_at: Some(1_500),
            ..timer(Some(5))
        };
        assert_eq!(
            timer_status(&ended, Some(&state(2)), 1_500),
            (true, "Ended".into())
        );
    }

    #[test]
    fn curly_quotes_become_straight() {
        assert_eq!(
            straight_quotes("content == \u{201c}go\u{201d}"),
            "content == \"go\""
        );
    }

    #[test]
    fn until_reads_local_times() {
        assert!(parse_local("2030-01-02 03:04").is_some());
        assert!(parse_local("2030-01-02T03:04").is_some());
        assert!(parse_local("tomorrow").is_none());
        assert_eq!(number(f64::NAN), Value::Null);
        assert_eq!(number(3600.0), json!(3600));
    }
}
