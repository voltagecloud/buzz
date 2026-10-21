//! The workflow editor dialog (`WorkflowEditor.tsx`, `WorkflowForm.tsx`,
//! `WorkflowConditions.tsx`, `WorkflowScheduleFields.tsx`,
//! `CronExpressionInput.tsx`, `WorkflowWebhookFields.tsx`).
//!
//! The editor is state its host owns and renders: the host keeps the
//! draft YAML and decides what saving means. Form mode edits a parsed
//! copy and writes YAML back on every change; YAML mode edits the text
//! itself, which is kept as written until the user changes it.

use super::controls::{self, Confirm, Handler};
use super::model::activation;
use super::model::conditions::{self, Condition};
use super::model::cron;
use super::model::editor::{Issue, IssueField, draft_issue};
use super::model::form::{
    ACTION_TYPES, ActionType, Header, Step, TRIGGER_TYPES, TriggerConfig, TriggerType,
    WorkflowForm, form_to_yaml, next_step_id, with_trigger_type, yaml_to_form,
};
use super::model::schedule::{self, FREQUENCIES, Frequency, ScheduleForm};
use super::model::yaml::{self as document, DocumentFields};
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, ButtonVariant, Icon, IconButton, IconButtonSize, IconName,
    Menu, MenuItem, MenuSeparator, MenuSize, Surface, Switch, TextArea, TextField, TypeStyled,
    tokens::{radius, shadow, space},
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// What is selected in the flow: the trigger, a step, or nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Trigger,
    Step(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Form,
    Yaml,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConditionMode {
    Basic,
    Advanced,
}

/// The dropdown open in the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OpenMenu {
    Actions,
    /// The insertion menu after a step (`-1`: after the trigger).
    Add(isize),
    StepAction,
    Trigger,
    Operator(String),
    AddCondition,
    Method,
    MonthDay,
}

/// A row in the header actions menu.
pub enum ActionItem {
    Item {
        id: &'static str,
        label: SharedString,
        icon: IconName,
        danger: bool,
        disabled: bool,
        run: Handler,
    },
    Separator,
    /// The "Enable" checkbox row, wherever the host places it.
    Enable,
}

/// What the host decides each render.
pub struct Props {
    pub read_only: bool,
    pub blocked: Option<SharedString>,
    pub busy: bool,
    pub locked: bool,
    pub create: bool,
    pub choose_channel: bool,
    /// The original YAML of a saved definition, for the enable warning.
    pub initial_yaml: Option<String>,
    /// Above the flow (the channel) or, while choosing a channel, the body.
    pub scope: Option<AnyElement>,
    /// The header actions menu; `None` hides it (create).
    pub actions: Option<Vec<ActionItem>>,
    pub status: Vec<AnyElement>,
    pub details: Vec<AnyElement>,
    /// Confirmations the host shows over the editor.
    pub children: Vec<AnyElement>,
}

/// The host of an editor: it owns the draft.
pub trait EditorHost: Sized + 'static {
    fn editor(&mut self) -> Option<&mut Editor>;
    fn editor_ref(&self) -> Option<&Editor>;
    fn draft_yaml(&self) -> String;
    fn draft_changed(&mut self, yaml: String, window: &mut Window, cx: &mut Context<Self>);
    fn save_draft(&mut self, window: &mut Window, cx: &mut Context<Self>);
    fn cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>);
    /// Invalid form-only values may serialize to unchanged YAML; they still
    /// count as unsaved changes.
    fn local_draft_risk(&mut self, _at_risk: bool, _cx: &mut Context<Self>) {}
}

enum Slot {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
}

/// The text inputs the editor shows, by purpose. Each follows the form
/// value while it is not focused.
#[derive(Default)]
struct Inputs {
    /// Each with its current `Apply`, type-erased, and change subscription.
    slots: HashMap<String, (Slot, Rc<dyn Any>, Subscription)>,
    used: HashSet<String>,
}

pub struct Editor {
    selection: Option<Selection>,
    mode: Mode,
    form_draft: Option<WorkflowForm>,
    form_yaml: String,
    activating: bool,
    editing_name: bool,
    mode_error: Option<String>,
    menu: Option<OpenMenu>,
    settings_open: bool,
    /// Expanded step accordions: (step id, "run" | "details").
    expanded: HashSet<(String, &'static str)>,
    condition_mode: Option<(TriggerType, ConditionMode)>,
    condition_mode_error: bool,
    replace_conditions: bool,
    force_custom_cron: Option<TriggerType>,
    cron_fields: [String; 5],
    cron_local: String,
    cron_error: Option<String>,
    inputs: Inputs,
    name_input: Option<(Entity<InputState>, Subscription)>,
    yaml_readonly: Option<bool>,
    risk: bool,
    /// The draft YAML as last parsed; renders reuse it until it changes.
    parsed: RefCell<Option<Rc<Parsed>>>,
    pub focus: FocusHandle,
    confirm_focus: FocusHandle,
}

const DIALOG_LAYER: usize = 1;

impl Editor {
    pub fn new(yaml: &str, create: bool, choose_channel: bool, cx: &mut App) -> Self {
        Self {
            selection: (create && !choose_channel).then_some(Selection::Trigger),
            mode: if yaml_to_form(yaml).is_ok() {
                Mode::Form
            } else {
                Mode::Yaml
            },
            form_draft: None,
            form_yaml: yaml.to_string(),
            activating: false,
            editing_name: false,
            mode_error: None,
            menu: None,
            settings_open: false,
            expanded: HashSet::new(),
            condition_mode: None,
            condition_mode_error: false,
            replace_conditions: false,
            force_custom_cron: None,
            cron_fields: Default::default(),
            cron_local: String::new(),
            cron_error: None,
            inputs: Inputs::default(),
            name_input: None,
            yaml_readonly: None,
            risk: false,
            parsed: RefCell::default(),
            focus: cx.focus_handle(),
            confirm_focus: cx.focus_handle(),
        }
    }

    /// Whether a menu, the inspector or a confirmation is open, which
    /// Escape closes before the editor.
    fn escape(&mut self) -> bool {
        if self.menu.take().is_some() {
            return true;
        }
        if self.activating {
            self.activating = false;
            return true;
        }
        if self.selection.take().is_some() {
            return true;
        }
        false
    }
}

/// The form the editor shows for `yaml`: the local draft while it still
/// writes this YAML, else a fresh parse.
fn current_form(editor: &Editor, yaml: &str) -> Option<WorkflowForm> {
    if editor.form_yaml == yaml
        && let Some(draft) = &editor.form_draft
    {
        return Some(draft.clone());
    }
    yaml_to_form(yaml).ok()
}

fn form_of<V: EditorHost>(this: &V) -> Option<WorkflowForm> {
    let yaml = this.draft_yaml();
    current_form(this.editor_ref()?, &yaml)
}

/// Writes form state as the draft (`mutateForm`).
fn mutate<V: EditorHost>(
    this: &mut V,
    form: WorkflowForm,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let yaml = form_to_yaml(&form);
    if let Some(editor) = this.editor() {
        editor.form_draft = Some(form);
        editor.form_yaml = yaml.clone();
    }
    this.draft_changed(yaml, window, cx);
}

/// Applies a header edit (`changeHeader`): the YAML edit, and the same
/// change to the form draft.
fn change_header<V: EditorHost>(
    this: &mut V,
    next: Option<String>,
    patch: impl FnOnce(&mut WorkflowForm),
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let Some(next) = next else {
        return;
    };
    if let Some(mut form) = form_of(this) {
        patch(&mut form);
        if let Some(editor) = this.editor() {
            editor.form_draft = Some(form);
            editor.form_yaml = next.clone();
        }
    }
    this.draft_changed(next, window, cx);
}

fn update_form<V: EditorHost>(
    this: &mut V,
    window: &mut Window,
    cx: &mut Context<V>,
    change: impl FnOnce(&mut WorkflowForm),
) {
    if let Some(mut form) = form_of(this) {
        change(&mut form);
        mutate(this, form, window, cx);
    }
}

fn update_step<V: EditorHost>(
    this: &mut V,
    id: &str,
    window: &mut Window,
    cx: &mut Context<V>,
    change: impl FnOnce(&mut Step),
) {
    let id = id.to_string();
    update_form(this, window, cx, |form| {
        if let Some(step) = form.steps.iter_mut().find(|step| step.id == id) {
            change(step);
        }
    });
}

fn set_menu<V: EditorHost>(this: &mut V, menu: Option<OpenMenu>, cx: &mut Context<V>) {
    if let Some(editor) = this.editor() {
        editor.menu = menu;
    }
    cx.notify();
}

fn toggle_menu<V: EditorHost>(this: &mut V, menu: OpenMenu, cx: &mut Context<V>) {
    let open = this.editor_ref().and_then(|e| e.menu.clone()) == Some(menu.clone());
    set_menu(this, (!open).then_some(menu), cx);
}

fn dismiss<V: EditorHost>(cx: &mut Context<V>) -> Dismiss {
    let host = cx.entity().downgrade();
    Rc::new(move |_, cx| {
        let _ = host.update(cx, |this, cx| set_menu(this, None, cx));
    })
}

fn select<V: EditorHost>(this: &mut V, selection: Option<Selection>, cx: &mut Context<V>) {
    if let Some(editor) = this.editor() {
        editor.selection = selection;
        editor.menu = None;
    }
    cx.notify();
}

// ---------------------------------------------------------------------------
// Inputs

/// What an input's change runs; replaced on every render.
type Apply<V> = Rc<dyn Fn(&mut V, String, &mut Window, &mut Context<V>)>;

/// The single- and multi-line input states, as `input` uses them.
trait TextState: EventEmitter<InputEvent> + Sized + 'static {
    fn of(slot: &Slot) -> Option<Entity<Self>>;
    fn slot(state: Entity<Self>) -> Slot;
    fn text(&self) -> SharedString;
    fn focused(&self, window: &Window, cx: &App) -> bool;
    fn replace(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>);
}

macro_rules! text_state {
    ($state:ty, $variant:ident) => {
        impl TextState for $state {
            fn of(slot: &Slot) -> Option<Entity<Self>> {
                match slot {
                    Slot::$variant(state) => Some(state.clone()),
                    _ => None,
                }
            }
            fn slot(state: Entity<Self>) -> Slot {
                Slot::$variant(state)
            }
            fn text(&self) -> SharedString {
                self.value()
            }
            fn focused(&self, window: &Window, cx: &App) -> bool {
                self.focus_handle(cx).contains_focused(window, cx)
            }
            fn replace(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
                self.set_value(value, window, cx);
            }
        }
    };
}

text_state!(InputState, Line);
text_state!(TextareaState, Area);

/// The input for `key`, following `value` while unfocused. `apply` is
/// swapped in on every render, so it never acts on what an earlier
/// render captured (a row index, the schedule, `disabled`).
fn input<V: EditorHost, S: TextState>(
    this: &mut V,
    key: String,
    value: &str,
    window: &mut Window,
    cx: &mut Context<V>,
    new: impl FnOnce(String, &mut Window, &mut Context<S>) -> S,
    apply: impl Fn(&mut V, String, &mut Window, &mut Context<V>) + 'static,
) -> Entity<S> {
    let apply: Apply<V> = Rc::new(apply);
    let existing = this.editor_ref().and_then(|editor| {
        let (slot, current, _) = editor.inputs.slots.get(&key)?;
        Some((S::of(slot)?, current.clone()))
    });
    let current = existing
        .as_ref()
        .and_then(|(_, current)| current.clone().downcast::<RefCell<Apply<V>>>().ok());
    let state = match existing.zip(current) {
        Some(((state, _), current)) => {
            *current.borrow_mut() = apply;
            if !state.read(cx).focused(window, cx) && state.read(cx).text() != value {
                let value = value.to_string();
                state.update(cx, |state, cx| state.replace(value, window, cx));
            }
            state
        }
        None => {
            let initial = value.to_string();
            let state = cx.new(|cx| new(initial, window, cx));
            let current = Rc::new(RefCell::new(apply));
            let subscription = cx.subscribe_in(&state, window, {
                let current = current.clone();
                move |this, state, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = state.read(cx).text().to_string();
                        let apply = current.borrow().clone();
                        apply(this, value, window, cx);
                        cx.notify();
                    }
                }
            });
            if let Some(editor) = this.editor() {
                editor
                    .inputs
                    .slots
                    .insert(key.clone(), (S::slot(state.clone()), current, subscription));
            }
            state
        }
    };
    if let Some(editor) = this.editor() {
        editor.inputs.used.insert(key);
    }
    state
}

/// A single-line input for `key`, following `value` while unfocused.
fn line<V: EditorHost>(
    this: &mut V,
    key: String,
    value: &str,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<V>,
    apply: impl Fn(&mut V, String, &mut Window, &mut Context<V>) + 'static,
) -> Entity<InputState> {
    let new = move |initial: String, window: &mut Window, cx: &mut Context<InputState>| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(initial)
    };
    input(this, key, value, window, cx, new, apply)
}

/// A multi-line input for `key`, following `value` while unfocused.
fn area<V: EditorHost>(
    this: &mut V,
    key: String,
    value: &str,
    window: &mut Window,
    cx: &mut Context<V>,
    apply: impl Fn(&mut V, String, &mut Window, &mut Context<V>) + 'static,
) -> Entity<TextareaState> {
    let new = |initial: String, window: &mut Window, cx: &mut Context<TextareaState>| {
        TextareaState::new(window, cx).default_value(initial)
    };
    input(this, key, value, window, cx, new, apply)
}

fn text_field(state: &Entity<InputState>, invalid: bool, disabled: bool) -> TextField {
    TextField::new(state)
        .surface(Surface::Floating)
        .invalid(invalid)
        .disabled(disabled)
}

// ---------------------------------------------------------------------------
// Rendering

/// What the draft YAML alone decides.
struct Parsed {
    yaml: String,
    fields: DocumentFields,
    form: Result<WorkflowForm, String>,
    issue: Option<Issue>,
    warning: Option<activation::Warning>,
}

impl Parsed {
    /// `editor`'s last parse when `yaml` is unchanged, else a new one.
    fn of(editor: &Editor, yaml: String) -> Rc<Self> {
        let mut cached = editor.parsed.borrow_mut();
        if let Some(parsed) = cached.as_ref().filter(|parsed| parsed.yaml == yaml) {
            return parsed.clone();
        }
        let parsed = Rc::new(Self {
            fields: document::read_fields(&yaml),
            form: yaml_to_form(&yaml),
            issue: draft_issue(&yaml),
            warning: activation::warning(&yaml),
            yaml,
        });
        *cached = Some(parsed.clone());
        parsed
    }
}

struct Derived {
    yaml: String,
    fields: DocumentFields,
    parsed: Result<WorkflowForm, String>,
    form: Option<WorkflowForm>,
    issue: Option<Issue>,
    warning: Option<activation::Warning>,
    condition_error: Option<&'static str>,
    header_error: Option<&'static str>,
    error: Option<SharedString>,
}

fn derive<V: EditorHost>(this: &V) -> Option<Derived> {
    let editor = this.editor_ref()?;
    let yaml = this.draft_yaml();
    let form = match (&editor.form_draft, editor.form_yaml == yaml) {
        (Some(draft), true) => Some(draft.clone()),
        _ => None,
    };
    let cached = Parsed::of(editor, yaml);
    let parsed = cached.form.clone();
    let form = form.or_else(|| parsed.clone().ok());
    let issue = cached.issue.clone();
    let condition_error = form
        .as_ref()
        .and_then(|form| form.trigger.condition_rows.as_ref())
        .and_then(|rows| rows.iter().find_map(conditions::row_error));
    let header_error = form
        .as_ref()
        .is_some_and(|form| {
            form.steps.iter().any(|step| {
                step.headers.as_ref().is_some_and(|headers| {
                    let names: HashSet<&str> = headers.iter().map(|h| h.name.as_str()).collect();
                    names.len() != headers.len()
                })
            })
        })
        .then_some("Header names must be unique.");
    let error = issue
        .as_ref()
        .map(|issue| SharedString::from(issue.message))
        .or(condition_error.map(SharedString::from))
        .or(header_error.map(SharedString::from));
    Some(Derived {
        yaml: cached.yaml.clone(),
        fields: cached.fields.clone(),
        parsed,
        form,
        issue,
        warning: cached.warning.clone(),
        condition_error,
        header_error,
        error,
    })
}

/// Renders the editor dialog in its modal layer.
pub fn render<V: EditorHost>(
    this: &mut V,
    props: Props,
    window: &mut Window,
    cx: &mut Context<V>,
) -> AnyElement {
    let Some(d) = derive(this) else {
        return div().into_any_element();
    };
    if let Some(editor) = this.editor() {
        editor.inputs.used.clear();
    }
    let c = *cx.colors();
    let editor = this.editor_ref().expect("editor");
    let mode = editor.mode;
    let editing_name = editor.editing_name;
    let selection = editor.selection.clone();
    let mode_error = editor.mode_error.clone();
    let activating = editor.activating;
    let settings_open = editor.settings_open;
    let menu = editor.menu.clone();
    let focus = editor.focus.clone();
    let confirm_focus = editor.confirm_focus.clone();

    // Unchanged YAML can still hold an invalid form-only value.
    let risk = editor.form_yaml == d.yaml
        && editor.form_draft.is_some()
        && (d.condition_error.is_some() || d.header_error.is_some());
    if risk != editor.risk {
        if let Some(editor) = this.editor() {
            editor.risk = risk;
        }
        this.local_draft_risk(risk, cx);
    }

    let showing_form = mode == Mode::Form && d.form.is_some();
    let disabled = props.read_only || props.busy || props.locked || props.choose_channel;
    let control_reason: Option<SharedString> = if props.read_only {
        Some("Only the author can change this workflow.".into())
    } else if let Some(blocked) = props.blocked.clone() {
        Some(blocked)
    } else if props.busy || props.locked {
        Some("Review Recent activity before editing.".into())
    } else if props.choose_channel {
        Some("Choose a channel before editing.".into())
    } else if !d.fields.editable {
        Some("Correct the YAML before changing the name or configuration.".into())
    } else {
        None
    };
    let warning = d.warning.clone();
    let steps_empty = d.form.as_ref().is_none_or(|form| form.steps.is_empty());
    let title = if props.create {
        "Create workflow"
    } else if props.read_only {
        "View workflow"
    } else {
        "Edit workflow"
    };

    // --- header
    let close = cx.listener(|this: &mut V, _, window, cx| this.cancel_draft(window, cx));
    let actions_open = menu == Some(OpenMenu::Actions);
    let enabled_checked = d.fields.is_enabled() && d.fields.editable;
    let enable_disabled = disabled || !d.fields.editable;
    // A new draft has no header actions (`headerActions={!create && …}`).
    let actions_menu = props.actions.filter(|_| !props.create).map(|items| {
        let mut list = Menu::new().size(MenuSize::Default);
        for item in items {
            list = match item {
                ActionItem::Separator => list.child(MenuSeparator::new()),
                ActionItem::Enable => {
                    let yaml = d.yaml.clone();
                    list.child(
                        MenuItem::new("workflow-enable", "Enable")
                            .icon(IconName::Lightning)
                            .icon_size(14.)
                            .checked(enabled_checked)
                            .disabled(enable_disabled)
                            .on_click(cx.listener(move |this: &mut V, _, window, cx| {
                                let enabled = !enabled_checked;
                                change_header(
                                    this,
                                    document::with_enabled(&yaml, enabled),
                                    |form| form.enabled = enabled,
                                    window,
                                    cx,
                                );
                            })),
                    )
                }
                ActionItem::Item {
                    id,
                    label,
                    icon,
                    danger,
                    disabled,
                    run,
                } => list.child(
                    MenuItem::new(id, label)
                        .icon(icon)
                        .icon_size(14.)
                        .disabled(disabled)
                        .when(danger, MenuItem::danger)
                        .on_click(cx.listener(move |this: &mut V, _, window, cx| {
                            set_menu(this, None, cx);
                            run(window, cx);
                        })),
                ),
            };
        }
        div()
            .relative()
            .child(
                IconButton::new("workflow-actions", IconName::DotsThree)
                    .size(IconButtonSize::Sm)
                    .surface(Surface::Floating)
                    .selected(actions_open)
                    .when(!actions_open, |b| b.tooltip("Workflow actions"))
                    .on_click(cx.listener(|this: &mut V, _, _, cx| {
                        toggle_menu(this, OpenMenu::Actions, cx)
                    })),
            )
            .when(actions_open, |this| {
                this.child(overlay::dropdown(window, Align::End, list, dismiss(cx)))
            })
    });
    let header = div()
        .flex()
        .items_start()
        .justify_between()
        .gap(space::S4)
        .child(div().text_label().child(title))
        .child(
            div()
                .flex()
                .flex_none()
                .gap(space::S2)
                .children(actions_menu)
                .child(
                    IconButton::new("workflow-close", IconName::X)
                        .size(IconButtonSize::Sm)
                        .surface(Surface::Floating)
                        .tooltip("Close editor")
                        .on_click(close),
                ),
        );

    // --- toolbar: name and enabled switch
    let name_issue = d
        .issue
        .as_ref()
        .filter(|issue| issue.field == Some(IssueField::Name));
    let name_input = this
        .editor_ref()
        .and_then(|e| e.name_input.as_ref().map(|(s, _)| s.clone()));
    let name = if let (true, Some(state)) = (editing_name, name_input) {
        controls::field(
            "Workflow name",
            None,
            if showing_form {
                name_issue.map(|issue| SharedString::from(issue.message))
            } else {
                None
            },
            div()
                .on_action(
                    cx.listener(|this: &mut V, _: &buzz_ui::Cancel, window, cx| {
                        if let Some(editor) = this.editor() {
                            editor.editing_name = false;
                            editor.name_input = None;
                            let focus = editor.focus.clone();
                            window.focus(&focus, cx);
                        }
                        cx.notify();
                    }),
                )
                .child(text_field(&state, false, disabled || !d.fields.editable)),
            cx,
        )
        .into_any_element()
    } else {
        div()
            .flex()
            .flex_col()
            .gap(space::S1)
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S2)
                    .min_w_0()
                    .child(
                        div().text_mono().truncate().child(
                            d.fields
                                .name
                                .clone()
                                .unwrap_or_else(|| "Untitled workflow".into()),
                        ),
                    )
                    .child(
                        IconButton::new("workflow-name-edit", IconName::PencilSimple)
                            .size(IconButtonSize::Sm)
                            .surface(Surface::Floating)
                            .tooltip("Edit workflow name")
                            .disabled(disabled || !d.fields.editable)
                            .on_click(cx.listener(|this: &mut V, _, window, cx| {
                                begin_name_edit(this, window, cx)
                            })),
                    ),
            )
            .children(name_issue.map(|issue| {
                div()
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child(issue.message)
            }))
            .into_any_element()
    };
    let enabled_switch = if props.create && d.fields.editable {
        let yaml = d.yaml.clone();
        let enabled = d.fields.is_enabled();
        let host = cx.entity().downgrade();
        let toggle = Rc::new(move |window: &mut Window, cx: &mut App| {
            let yaml = yaml.clone();
            let _ = host.update(cx, |this, cx| {
                change_header(
                    this,
                    document::with_enabled(&yaml, !enabled),
                    |form| form.enabled = !enabled,
                    window,
                    cx,
                );
            });
        });
        // The label leads the switch, as buzz-app's Switch draws it.
        Some(
            div()
                .id("workflow-enabled-row")
                .flex()
                .items_center()
                .gap(space::S3)
                .text_body()
                .text_color(if disabled {
                    c.text_unavailable
                } else {
                    c.text_standard
                })
                .when(!disabled, |this| {
                    this.cursor_pointer()
                        .on_click(move |_, window, cx| toggle(window, cx))
                })
                .child("Enabled in configuration")
                .child(
                    // The row takes the click.
                    Switch::new("workflow-enabled", enabled).disabled(disabled),
                )
                .into_any_element(),
        )
    } else if !d.fields.editable {
        Some(controls::note("Unreadable configuration", cx).into_any_element())
    } else {
        None
    };
    let toolbar = div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap(space::S3)
        .child(div().flex_1().min_w(rems(15.)).child(name))
        .children(enabled_switch);

    // --- body
    let form_layout = mode == Mode::Form && d.form.is_some() && !props.choose_channel;
    let mut body = div()
        .id("workflow-editor-body")
        .flex()
        .flex_col()
        .gap(space::S4)
        .flex_1()
        .min_h_0()
        .when(!form_layout, |this| this.overflow_y_scroll());
    if let Some(error) = mode_error {
        body = body.child(controls::danger(
            format!("Cannot switch editor view: {error}"),
            cx,
        ));
    }
    let mut scope = props.scope;
    if props.choose_channel {
        body = body.children(scope.take());
    } else if let (Mode::Form, Some(form)) = (mode, d.form.clone()) {
        body = body.child(builder(
            this,
            &form,
            d.issue.as_ref(),
            disabled,
            scope.take(),
            selection.clone(),
            window,
            cx,
        ));
    } else {
        let state = area(
            this,
            "yaml".into(),
            &d.yaml,
            window,
            cx,
            |this, value, window, cx| {
                if let Some(editor) = this.editor() {
                    editor.form_draft = None;
                    editor.mode_error = None;
                }
                this.draft_changed(value, window, cx);
            },
        );
        if this
            .editor_ref()
            .is_some_and(|e| e.yaml_readonly != Some(disabled))
        {
            state.update(cx, |state, cx| state.set_readonly(disabled, cx));
            if let Some(editor) = this.editor() {
                editor.yaml_readonly = Some(disabled);
            }
        }
        body = body.child(controls::field(
            "Workflow YAML",
            Some("Original text is kept until you edit. Form changes may reformat YAML. Schedules use UTC.".into()),
            d.error.clone(),
            TextArea::new(&state)
                .code()
                .rows(18)
                .surface(Surface::Floating)
                .invalid(d.error.is_some()),
            cx,
        ));
    }
    let issue_elsewhere = d.issue.as_ref().is_none_or(|issue| issue.field.is_none())
        || d.condition_error.is_some()
        || d.header_error.is_some();
    if showing_form
        && issue_elsewhere
        && !steps_empty
        && let Some(error) = d.error.clone()
    {
        body = body.child(controls::danger(error, cx));
    }

    // --- status
    let mut status = div()
        .id("workflow-editor-status")
        .flex()
        .flex_col()
        .gap(space::S2)
        .flex_none()
        .max_h(window.viewport_size().height * 0.3)
        .overflow_y_scroll();
    let has_status = (!props.read_only && control_reason.is_some()) || !props.status.is_empty();
    if !props.read_only
        && let Some(reason) = &control_reason
    {
        status = status.child(controls::note(reason.clone(), cx));
    }
    status = status.children(props.status);

    // --- settings accordion
    let settings = (!props.choose_channel).then(|| {
        let mut content = div().flex().flex_col().gap(space::S4).pt(space::S4);
        if let Some(form) = &d.form {
            let state = line(
                this,
                "description".into(),
                &form.description,
                "",
                window,
                cx,
                |this, value, window, cx| {
                    update_form(this, window, cx, |form| form.description = value);
                },
            );
            content = content.child(controls::field(
                "Description",
                None,
                None,
                text_field(&state, false, disabled),
                cx,
            ));
        }
        content = content.children(props.details);
        div()
            .flex()
            .flex_col()
            .child(controls::accordion_trigger(
                "workflow-settings",
                "Workflow settings & activity",
                None,
                settings_open,
                {
                    let host = cx.entity().downgrade();
                    move |_, cx| {
                        let _ = host.update(cx, |this, cx| {
                            if let Some(editor) = this.editor() {
                                editor.settings_open = !editor.settings_open;
                            }
                            cx.notify();
                        });
                    }
                },
                cx,
            ))
            .when(settings_open, |this| {
                this.child(div().p(space::S2).child(content))
            })
    });

    // --- footer
    let tabs = {
        let host = cx.entity().downgrade();
        controls::panel_tabs(
            "workflow-mode",
            &[("form", "Form"), ("yaml", "YAML")],
            if mode == Mode::Form { "form" } else { "yaml" },
            move |next, _, cx| {
                let _ = host.update(cx, |this, cx| switch_mode(this, next, cx));
            },
        )
    };
    let primary_label = if props.busy {
        if props.create {
            "Creating…"
        } else {
            "Saving…"
        }
    } else if showing_form && steps_empty {
        "Add step"
    } else if props.create {
        "Create workflow"
    } else {
        "Save changes"
    };
    let primary_disabled = disabled
        || (props.blocked.is_some() && !props.choose_channel)
        || (d.error.is_some() && !(showing_form && steps_empty));
    let blocked = props.blocked.is_some();
    let read_only = props.read_only;
    let busy = props.busy;
    let locked = props.locked;
    let initial = props.initial_yaml.clone();
    let footer = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(space::S3)
        .child(tabs)
        .child(div().flex_1())
        .when(!props.read_only, |this| {
            this.child(
                Button::new("workflow-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .on_click(
                        cx.listener(|this: &mut V, _, window, cx| this.cancel_draft(window, cx)),
                    ),
            )
            .child(
                Button::new("workflow-primary", primary_label)
                    .prominent()
                    .surface(Surface::Floating)
                    .disabled(primary_disabled)
                    .on_click(cx.listener(move |this: &mut V, _, window, cx| {
                        let Some(d) = derive(this) else { return };
                        let showing_form = this.editor_ref().is_some_and(|e| e.mode == Mode::Form)
                            && d.form.is_some();
                        if let Some(form) = d
                            .form
                            .clone()
                            .filter(|f| showing_form && f.steps.is_empty())
                        {
                            let mut form = form;
                            form.steps = vec![Step::new("step_1".into(), ActionType::SendMessage)];
                            mutate(this, form, window, cx);
                            select(this, Some(Selection::Step("step_1".into())), cx);
                            return;
                        }
                        if read_only || busy || locked || blocked || d.error.is_some() {
                            return;
                        }
                        let was_enabled = initial
                            .as_deref()
                            .is_some_and(|yaml| document::read_fields(yaml).is_enabled());
                        if d.fields.is_enabled() && !was_enabled && d.warning.is_some() {
                            if let Some(editor) = this.editor() {
                                editor.activating = true;
                            }
                            cx.notify();
                        } else {
                            this.save_draft(window, cx);
                        }
                    })),
            )
        });

    // --- confirmation
    let activation = activating.then(|| {
        let host = cx.entity().downgrade();
        let cancel_host = host.clone();
        Confirm::new(
            warning
                .as_ref()
                .map_or("Save this workflow enabled?", |w| w.title),
            warning.as_ref().map_or_else(
                || "The relay may run this workflow automatically when its trigger matches. Saving does not prove a run succeeded.".to_string(),
                |w| w.description.clone(),
            ),
            "Save enabled workflow",
        )
        .render(
            "workflow-activate",
            &confirm_focus,
            Rc::new(move |window, cx| {
                let _ = host.update(cx, |this, cx| {
                    if let Some(editor) = this.editor() {
                        editor.activating = false;
                    }
                    this.save_draft(window, cx);
                });
            }),
            Rc::new(move |_, cx| {
                let _ = cancel_host.update(cx, |this, cx| {
                    if let Some(editor) = this.editor() {
                        editor.activating = false;
                    }
                    cx.notify();
                });
            }),
            window,
            cx,
        )
    });

    // Inputs no longer shown are dropped, so a returning field reads the
    // form again.
    if let Some(editor) = this.editor() {
        let used = std::mem::take(&mut editor.inputs.used);
        editor.inputs.slots.retain(|key, _| used.contains(key));
    }

    let viewport = window.viewport_size();
    let frame = div()
        .id("workflow-editor")
        .track_focus(&focus)
        .key_context(buzz_ui::DIALOG_CONTEXT)
        .on_action(
            cx.listener(|this: &mut V, _: &buzz_ui::Cancel, window, cx| {
                let handled = this.editor().is_some_and(Editor::escape);
                if handled {
                    cx.notify();
                } else {
                    this.cancel_draft(window, cx);
                }
            }),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .flex()
        .flex_col()
        .gap(space::S4)
        .w(viewport.width - px(48.))
        .h(viewport.height - px(48.))
        .p(space::S6)
        .bg(c.surface_popover)
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::PANEL)
        .shadow(shadow::sm())
        .text_color(c.text_standard)
        .text_body()
        .child(header)
        .child(toolbar)
        .child(body)
        .when(has_status, |this| this.child(status))
        .children(settings)
        .child(footer);
    let content = div()
        .child(frame)
        .children(activation)
        .children(props.children);
    // The editor holds focus while nothing inside it does.
    if !focus.contains_focused(window, cx) && !confirm_focus.is_focused(window) {
        let anything = window.focused(cx).is_some();
        if !anything {
            window.focus(&focus, cx);
        }
    }
    controls::layer(window, content.into_any_element(), DIALOG_LAYER)
}

/// Opens the inline name field (the pencil): Enter or leaving it commits,
/// Escape reverts.
fn begin_name_edit<V: EditorHost>(this: &mut V, window: &mut Window, cx: &mut Context<V>) {
    let name = document::read_fields(&this.draft_yaml())
        .name
        .unwrap_or_default();
    let state = cx.new(|cx| InputState::new(window, cx).default_value(name));
    let subscription = cx.subscribe_in(
        &state,
        window,
        |this: &mut V, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                commit_name(this, window, cx);
            }
        },
    );
    let handle = state.read(cx).focus_handle(cx);
    window.focus(&handle, cx);
    state.update(cx, |state, cx| state.select_all(window, cx));
    if let Some(editor) = this.editor() {
        editor.editing_name = true;
        editor.name_input = Some((state, subscription));
    }
    cx.notify();
}

fn commit_name<V: EditorHost>(this: &mut V, window: &mut Window, cx: &mut Context<V>) {
    let Some(editor) = this.editor() else { return };
    let Some((state, _)) = editor.name_input.take().filter(|_| editor.editing_name) else {
        return;
    };
    editor.editing_name = false;
    let focus = editor.focus.clone();
    let name = state.read(cx).value().to_string();
    let yaml = this.draft_yaml();
    change_header(
        this,
        document::with_name(&yaml, &name),
        |form| form.name = name.clone(),
        window,
        cx,
    );
    window.focus(&focus, cx);
}

/// The Form/YAML switch (`onValueChange` of the editor mode tabs).
fn switch_mode<V: EditorHost>(this: &mut V, next: &str, cx: &mut Context<V>) {
    let Some(d) = derive(this) else { return };
    let Some(editor) = this.editor() else { return };
    if next == "yaml" && (d.condition_error.is_some() || d.header_error.is_some()) {
        editor.mode_error = Some(
            "Correct the invalid condition or duplicate header before switching views.".into(),
        );
    } else if next == "form" && d.form.is_none() {
        editor.mode_error = d.parsed.err();
    } else {
        editor.mode_error = None;
        editor.selection = None;
        editor.mode = if next == "form" {
            Mode::Form
        } else {
            Mode::Yaml
        };
    }
    cx.notify();
}

// ---------------------------------------------------------------------------
// The flow and its inspector

fn trigger_icon(on: TriggerType) -> IconName {
    match on {
        TriggerType::MessagePosted => IconName::ChatCircle,
        TriggerType::ReactionAdded => IconName::Smiley,
        TriggerType::DiffPosted => IconName::GitPullRequest,
        TriggerType::Schedule => IconName::Calendar,
        TriggerType::Webhook => IconName::WebhooksLogo,
    }
}

fn step_summary(step: &Step) -> String {
    match step.action {
        ActionType::SendMessage => match step.text.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(text) => format!("“{text}”"),
            None => "Write a message…".into(),
        },
        ActionType::Delay => step
            .duration
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "Set a duration…".into()),
        ActionType::CallWebhook => step
            .url
            .clone()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "Set a URL…".into()),
    }
}

/// A flow node: 64px pill row with a round icon and a caption over a label.
fn node(
    id: impl Into<ElementId>,
    icon: AnyElement,
    caption: String,
    label: String,
    selected: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let c = cx.colors();
    let ix = c.interaction(Surface::Floating);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(space::S2)
        .w_full()
        .min_h(rems(4.))
        .px(space::S4)
        .py(space::S1)
        .rounded_full()
        .cursor_pointer()
        .when(selected, |this| this.bg(ix.selected))
        .when(!selected, |this| this.hover(move |s| s.bg(ix.row_hover)))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(rems(2.25))
                .rounded_full()
                .bg(c.surface_inset)
                .child(icon),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::S1)
                .min_w_0()
                .child(
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child(caption),
                )
                .child(div().text_label().truncate().child(label)),
        )
        .on_click(move |_, window, cx| on_click(window, cx))
}

#[allow(clippy::too_many_arguments)]
fn builder<V: EditorHost>(
    this: &mut V,
    form: &WorkflowForm,
    issue: Option<&Issue>,
    disabled: bool,
    scope: Option<AnyElement>,
    selection: Option<Selection>,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Div {
    let c = *cx.colors();
    let menu = this.editor_ref().and_then(|e| e.menu.clone());
    let step_index = match &selection {
        Some(Selection::Step(id)) => form.steps.iter().position(|s| &s.id == id),
        _ => None,
    };
    let inspector_open = selection == Some(Selection::Trigger) || step_index.is_some();
    let host = cx.entity().downgrade();
    let steps = form.steps.len();
    let trigger_selected = selection == Some(Selection::Trigger);
    let mut flow = div()
        .flex()
        .flex_col()
        .items_stretch()
        .w_full()
        .max_w(rems(25.))
        .mx_auto()
        .child({
            let host = host.clone();
            node(
                "workflow-node-trigger",
                Icon::new(trigger_icon(form.trigger.on))
                    .size(20.)
                    .into_any_element(),
                "TRIGGER".into(),
                form.trigger.on.label().into(),
                trigger_selected,
                move |_, cx| {
                    let _ = host.update(cx, |this, cx| select(this, Some(Selection::Trigger), cx));
                },
                cx,
            )
        })
        .child(connector(
            -1,
            steps,
            menu == Some(OpenMenu::Add(-1)),
            disabled,
            window,
            cx,
        ));
    for (index, step) in form.steps.iter().enumerate() {
        let selected = selection == Some(Selection::Step(step.id.clone()));
        let id = step.id.clone();
        let host = host.clone();
        flow = flow
            .child(node(
                SharedString::from(format!("workflow-node-{}", step.id)),
                div()
                    .text_body_sm()
                    .child((index + 1).to_string())
                    .into_any_element(),
                step.action.label().to_uppercase(),
                step_summary(step),
                selected,
                move |_, cx| {
                    let id = id.clone();
                    let _ = host.update(cx, |this, cx| select(this, Some(Selection::Step(id)), cx));
                },
                cx,
            ))
            .child(connector(
                index as isize,
                steps,
                menu == Some(OpenMenu::Add(index as isize)),
                disabled,
                window,
                cx,
            ));
    }
    let mut canvas = div()
        .id("workflow-canvas")
        .flex()
        .flex_col()
        .items_center()
        .min_w_0()
        .flex_1()
        .h_full()
        .overflow_y_scroll()
        .px(space::S4)
        .py(space::S6)
        .children(scope.map(|scope| div().mb(space::S6).child(scope)))
        .child(flow);
    if form.steps.is_empty() {
        canvas = canvas.child(
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("Add a step to decide what happens next."),
        );
    }
    if let Some(issue) = issue
        && let Some(index) = issue.step
        && form.steps.get(index).is_some()
        && Some(index) != step_index
    {
        let id = form.steps[index].id.clone();
        canvas = canvas.child(
            Button::new(
                "workflow-review-step",
                format!("Review step {}: {}", index + 1, issue.message),
            )
            .ghost()
            .surface(Surface::Floating)
            .on_click(cx.listener(move |this: &mut V, _, _, cx| {
                select(this, Some(Selection::Step(id.clone())), cx)
            })),
        );
    }
    let mut builder = div()
        .flex()
        .flex_1()
        .min_h_0()
        .gap(space::S6)
        .w_full()
        .child(canvas);
    if inspector_open {
        builder = builder.child(inspector(
            this, form, issue, disabled, step_index, window, cx,
        ));
    }
    builder
}

/// The arrow between nodes, with its insertion menu (`addMenu`). The
/// last connector always shows its plus; the others on hover.
fn connector<V: EditorHost>(
    after: isize,
    steps: usize,
    open: bool,
    disabled: bool,
    window: &Window,
    cx: &mut Context<V>,
) -> Stateful<Div> {
    let c = *cx.colors();
    let terminal = after == steps as isize - 1;
    let label = if after < 0 {
        "Add step".to_string()
    } else {
        format!("Add after Step {}", after + 1)
    };
    let item = |id: &'static str,
                label: &'static str,
                icon: IconName,
                action: ActionType,
                cx: &mut Context<V>| {
        MenuItem::new(id, label)
            .icon(icon)
            .icon_size(14.)
            .size(MenuSize::Compact)
            .on_click(cx.listener(move |this: &mut V, _, window, cx| {
                add_step(this, after, action, window, cx)
            }))
    };
    let group: SharedString = format!("workflow-connector-{after}").into();
    let menu = open.then(|| {
        Menu::new()
            .size(MenuSize::Compact)
            .child(item(
                "add-send",
                "Add Send Message",
                IconName::ChatCircle,
                ActionType::SendMessage,
                cx,
            ))
            .child(item(
                "add-delay",
                "Add Delay",
                IconName::Timer,
                ActionType::Delay,
                cx,
            ))
            .child(item(
                "add-webhook",
                "Add Call Webhook",
                IconName::WebhooksLogo,
                ActionType::CallWebhook,
                cx,
            ))
    });
    div()
        .id(group.clone())
        .group(group.clone())
        .relative()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .h(rems(3.))
        .text_color(c.text_subtle)
        .when(!terminal, |this| {
            this.child(
                div()
                    .absolute()
                    .when(open, |this| this.opacity(0.))
                    .group_hover(group.clone(), |s| s.opacity(0.))
                    .child(Icon::new(IconName::ArrowDown).size(18.)),
            )
        })
        .child(
            div()
                .relative()
                .when(!terminal && !open, |this| {
                    this.opacity(0.)
                        .group_hover(group.clone(), |s| s.opacity(1.))
                })
                .child(
                    IconButton::new(
                        SharedString::from(format!("workflow-add-{after}")),
                        IconName::Plus,
                    )
                    .size(IconButtonSize::Sm)
                    .variant(ButtonVariant::Subtle)
                    .surface(Surface::Floating)
                    .disabled(disabled)
                    .tooltip(label)
                    .on_click(cx.listener(move |this: &mut V, _, _, cx| {
                        toggle_menu(this, OpenMenu::Add(after), cx)
                    })),
                )
                .when_some(menu, |this, menu| {
                    this.child(overlay::dropdown(window, Align::Start, menu, dismiss(cx)))
                }),
        )
}

fn add_step<V: EditorHost>(
    this: &mut V,
    after: isize,
    action: ActionType,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let Some(mut form) = form_of(this) else {
        return;
    };
    let step = Step::new(next_step_id(&form.steps), action);
    let id = step.id.clone();
    form.steps.insert((after + 1) as usize, step);
    mutate(this, form, window, cx);
    select(this, Some(Selection::Step(id)), cx);
}

#[allow(clippy::too_many_arguments)]
fn inspector<V: EditorHost>(
    this: &mut V,
    form: &WorkflowForm,
    issue: Option<&Issue>,
    disabled: bool,
    step_index: Option<usize>,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Stateful<Div> {
    let c = *cx.colors();
    let menu = this.editor_ref().and_then(|e| e.menu.clone());
    let selected = step_index.and_then(|index| form.steps.get(index).cloned());
    let heading_select = match &selected {
        Some(step) => {
            let id = step.id.clone();
            let current = step.action;
            controls::select_field(
                "workflow-step-action",
                "Step action",
                Some(current.label().into()),
                ACTION_TYPES
                    .iter()
                    .map(|action| (SharedString::from(action.label()), *action == current))
                    .collect(),
                menu == Some(OpenMenu::StepAction),
                disabled,
                |this: &mut V, cx| toggle_menu(this, OpenMenu::StepAction, cx),
                move |this: &mut V, index, window, cx| {
                    set_menu(this, None, cx);
                    let action = ACTION_TYPES[index];
                    if disabled || action == current {
                        return;
                    }
                    update_step(this, &id, window, cx, |step| {
                        *step = step.with_action(action)
                    });
                },
                dismiss(cx),
                window,
                cx,
            )
        }
        None => {
            let current = form.trigger.on;
            controls::select_field(
                "workflow-trigger",
                "Trigger",
                Some(current.label().into()),
                TRIGGER_TYPES
                    .iter()
                    .map(|on| (SharedString::from(on.label()), *on == current))
                    .collect(),
                menu == Some(OpenMenu::Trigger),
                disabled,
                |this: &mut V, cx| toggle_menu(this, OpenMenu::Trigger, cx),
                move |this: &mut V, index, window, cx| {
                    set_menu(this, None, cx);
                    let on = TRIGGER_TYPES[index];
                    if disabled || on == current {
                        return;
                    }
                    update_form(this, window, cx, |form| {
                        let mut next = with_trigger_type(form, on);
                        if on == TriggerType::Schedule {
                            next.trigger = schedule::default_trigger();
                        }
                        *form = next;
                    });
                },
                dismiss(cx),
                window,
                cx,
            )
        }
    };
    let heading = div()
        .flex()
        .items_start()
        .justify_between()
        .gap(space::S2)
        .mb(space::S5)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .children(step_index.filter(|_| selected.is_some()).map(|index| {
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child(format!("STEP {}", index + 1))
                }))
                .child(heading_select),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(space::S1)
                .when_some(step_index.filter(|_| selected.is_some()), |this, index| {
                    this.child(
                        IconButton::new("workflow-remove-step", IconName::Trash)
                            .size(IconButtonSize::Sm)
                            .surface(Surface::Floating)
                            .tooltip("Remove step")
                            .disabled(disabled)
                            .on_click(cx.listener(move |this: &mut V, _, window, cx| {
                                remove_step(this, index, disabled, window, cx)
                            })),
                    )
                })
                .child(
                    IconButton::new("workflow-close-inspector", IconName::X)
                        .size(IconButtonSize::Sm)
                        .surface(Surface::Floating)
                        .tooltip("Close inspector")
                        .on_click(cx.listener(|this: &mut V, _, _, cx| select(this, None, cx))),
                ),
        );
    let fields = match selected {
        Some(step) => step_fields(
            this,
            form,
            &step,
            step_index.unwrap_or(0),
            issue,
            disabled,
            window,
            cx,
        ),
        None => trigger_fields(this, form, disabled, window, cx),
    };
    div()
        .id("workflow-inspector")
        .flex()
        .flex_col()
        .flex_none()
        .w(rems(24.))
        .min_w(rems(18.))
        .h_full()
        .overflow_y_scroll()
        .p(space::S5)
        .bg(c.surface_panel)
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::PANEL)
        .child(heading)
        .child(fields)
}

fn remove_step<V: EditorHost>(
    this: &mut V,
    index: usize,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    if disabled {
        return;
    }
    let Some(mut form) = form_of(this) else {
        return;
    };
    if index >= form.steps.len() {
        return;
    }
    let neighbor = index
        .checked_sub(1)
        .and_then(|before| form.steps.get(before))
        .or_else(|| form.steps.get(index + 1))
        .map(|step| step.id.clone());
    form.steps.remove(index);
    mutate(this, form, window, cx);
    select(
        this,
        Some(neighbor.map_or(Selection::Trigger, Selection::Step)),
        cx,
    );
}

fn trigger_fields<V: EditorHost>(
    this: &mut V,
    form: &WorkflowForm,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Div {
    let mut fields = div().flex().flex_col().gap(space::S4);
    if form.trigger.on == TriggerType::ReactionAdded {
        let state = line(
            this,
            "trigger:emoji".into(),
            form.trigger.emoji.as_deref().unwrap_or(""),
            "",
            window,
            cx,
            |this, value, window, cx| {
                update_form(this, window, cx, |form| form.trigger.emoji = Some(value));
            },
        );
        fields = fields.child(controls::field(
            "Emoji (optional)",
            None,
            None,
            text_field(&state, false, disabled),
            cx,
        ));
    }
    match form.trigger.on {
        TriggerType::Schedule => fields.child(schedule_fields(this, &form.trigger, disabled, window, cx)),
        TriggerType::Webhook => fields.child(
            div()
                .text_body_sm()
                .text_color(cx.colors().text_subtle)
                .child("A unique URL is generated after creation. Its address and one-time secret are shown once, after the first save."),
        ),
        _ => fields.child(conditions_fields(this, &form.trigger, disabled, window, cx)),
    }
}

// --- conditions

fn conditions_fields<V: EditorHost>(
    this: &mut V,
    trigger: &TriggerConfig,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Div {
    let c = *cx.colors();
    let expression = trigger.filter.clone().unwrap_or_default();
    let parsed = conditions::parse_all(&expression, trigger.on);
    let editor = this.editor_ref().expect("editor");
    let mode = match editor.condition_mode {
        Some((on, mode)) if on == trigger.on => mode,
        _ => {
            let mode = if parsed.is_some() {
                ConditionMode::Basic
            } else {
                ConditionMode::Advanced
            };
            if let Some(editor) = this.editor() {
                editor.condition_mode = Some((trigger.on, mode));
                editor.condition_mode_error = false;
                editor.replace_conditions = false;
            }
            mode
        }
    };
    let editor = this.editor_ref().expect("editor");
    let mode_error = editor.condition_mode_error;
    let replace = editor.replace_conditions;
    let menu = editor.menu.clone();
    let confirm_focus = editor.confirm_focus.clone();
    let rows = trigger
        .condition_rows
        .clone()
        .or(parsed.clone())
        .unwrap_or_default();
    let fields = conditions::fields_for(trigger.on);
    let label_of = |field: &str| {
        fields
            .iter()
            .find(|f| f.value == field)
            .map_or("", |f| f.label)
    };
    let on = trigger.on;
    let has_parse = parsed.is_some();
    let rows_invalid = rows.iter().any(|row| conditions::row_error(row).is_some());
    let host = cx.entity().downgrade();
    let tabs = controls::panel_tabs(
        "workflow-condition-mode",
        &[("basic", "Basic"), ("advanced", "Advanced")],
        if mode == ConditionMode::Basic {
            "basic"
        } else {
            "advanced"
        },
        move |next, _, cx| {
            let _ = host.update(cx, |this, cx| {
                if let Some(editor) = this.editor() {
                    if next == "advanced" && rows_invalid {
                        editor.condition_mode_error = true;
                    } else {
                        editor.condition_mode_error = false;
                        if next == "basic" && !has_parse {
                            editor.replace_conditions = true;
                        } else {
                            editor.condition_mode = Some((
                                on,
                                if next == "basic" {
                                    ConditionMode::Basic
                                } else {
                                    ConditionMode::Advanced
                                },
                            ));
                        }
                    }
                }
                cx.notify();
            });
        },
    );
    let mut out = div().flex().flex_col().gap(space::S4).child(tabs);
    if mode_error && rows_invalid {
        out = out.child(controls::danger(
            "Complete or remove the invalid condition before switching to Advanced.",
            cx,
        ));
    }
    if mode == ConditionMode::Advanced {
        let state = area(
            this,
            "trigger:filter".into(),
            &expression,
            window,
            cx,
            |this, value, window, cx| {
                update_form(this, window, cx, |form| {
                    form.trigger.filter = Some(value);
                    form.trigger.condition_rows = None;
                });
            },
        );
        out = out.child(controls::field(
            "Trigger condition (optional)",
            Some("An evalexpr expression; leave empty to match every event of this type.".into()),
            None,
            TextArea::new(&state)
                .code()
                .rows(4)
                .surface(Surface::Floating)
                .disabled(disabled),
            cx,
        ));
    } else {
        out = out.child(controls::note(
            if rows.is_empty() {
                "Runs for every matching event. Add a condition to narrow it down."
            } else {
                "All conditions must match."
            },
            cx,
        ));
        for (index, row) in rows.iter().enumerate() {
            let label = label_of(&row.field);
            let rows_for_remove = rows.clone();
            let mut block = div().flex().flex_col().gap(space::S3).child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_label_sm().child(label))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("workflow-condition-remove-{}", row.field)),
                            IconName::Trash,
                        )
                        .size(IconButtonSize::Sm)
                        .surface(Surface::Floating)
                        .tooltip(format!("Remove {label} condition"))
                        .disabled(disabled)
                        .on_click(cx.listener(
                            move |this: &mut V, _, window, cx| {
                                let mut next = rows_for_remove.clone();
                                next.remove(index);
                                set_conditions(this, next, disabled, window, cx);
                            },
                        )),
                    ),
            );
            let operators = conditions::operators_for(&row.field);
            let rows_for_operator = rows.clone();
            let field_key = row.field.clone();
            block = block.child(controls::select_field(
                SharedString::from(format!("workflow-condition-op-{}", row.field)),
                format!("{label} comparison"),
                Some(row.operator.label().into()),
                operators
                    .iter()
                    .map(|op| (SharedString::from(op.label()), *op == row.operator))
                    .collect(),
                menu == Some(OpenMenu::Operator(row.field.clone())),
                disabled,
                move |this: &mut V, cx| {
                    toggle_menu(this, OpenMenu::Operator(field_key.clone()), cx)
                },
                move |this: &mut V, picked, window, cx| {
                    set_menu(this, None, cx);
                    let mut next = rows_for_operator.clone();
                    next[index].operator = conditions::operators_for(&next[index].field)[picked];
                    set_conditions(this, next, disabled, window, cx);
                },
                dismiss(cx),
                window,
                cx,
            ));
            if row.operator.needs_value() {
                let rows_for_value = rows.clone();
                let state = line(
                    this,
                    format!("condition:{}", row.field),
                    &row.value,
                    "",
                    window,
                    cx,
                    move |this, value, window, cx| {
                        // Read the rows again: another edit may have come first.
                        let current = form_of(this)
                            .and_then(|form| {
                                let parsed = conditions::parse_all(
                                    form.trigger.filter.as_deref().unwrap_or(""),
                                    form.trigger.on,
                                );
                                form.trigger.condition_rows.or(parsed)
                            })
                            .unwrap_or_else(|| rows_for_value.clone());
                        let mut next = current;
                        if let Some(row) = next.get_mut(index) {
                            row.value = value;
                        }
                        set_conditions(this, next, disabled, window, cx);
                    },
                );
                let error = conditions::row_error(row).map(SharedString::from);
                block = block.child(controls::field(
                    "Value",
                    None,
                    error.clone(),
                    text_field(&state, error.is_some(), disabled),
                    cx,
                ));
            }
            out = out.child(block);
        }
        let missing: Vec<conditions::Field> = fields
            .iter()
            .filter(|field| !rows.iter().any(|row| row.field == field.value))
            .copied()
            .collect();
        if !missing.is_empty() {
            let rows_for_add = rows.clone();
            let options = missing.clone();
            out = out.child(controls::select_field(
                "workflow-condition-add",
                "Add condition",
                None,
                missing
                    .iter()
                    .map(|field| (SharedString::from(field.label), false))
                    .collect(),
                menu == Some(OpenMenu::AddCondition),
                disabled,
                |this: &mut V, cx| toggle_menu(this, OpenMenu::AddCondition, cx),
                move |this: &mut V, picked, window, cx| {
                    set_menu(this, None, cx);
                    let field = options[picked].value;
                    let mut next = rows_for_add.clone();
                    next.push(Condition {
                        field: field.into(),
                        operator: conditions::default_operator(field),
                        value: String::new(),
                    });
                    set_conditions(this, next, disabled, window, cx);
                },
                dismiss(cx),
                window,
                cx,
            ));
        }
    }
    if replace {
        let host = cx.entity().downgrade();
        let cancel = host.clone();
        out = out.child(
            Confirm::new(
                "Replace with basic filters?",
                "This expression cannot be represented by Basic conditions. Replacing it clears the expression; cancel to keep it unchanged.",
                "Replace with basic filters",
            )
            .render(
                "workflow-replace-conditions",
                &confirm_focus,
                Rc::new(move |window, cx| {
                    let _ = host.update(cx, |this, cx| {
                        set_conditions(this, Vec::new(), disabled, window, cx);
                        if let Some(editor) = this.editor() {
                            editor.condition_mode = Some((on, ConditionMode::Basic));
                            editor.replace_conditions = false;
                        }
                        cx.notify();
                    });
                }),
                Rc::new(move |_, cx| {
                    let _ = cancel.update(cx, |this, cx| {
                        if let Some(editor) = this.editor() {
                            editor.replace_conditions = false;
                        }
                        cx.notify();
                    });
                }),
                window,
                cx,
            ),
        );
    }
    let _ = c;
    out
}

fn set_conditions<V: EditorHost>(
    this: &mut V,
    rows: Vec<Condition>,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    if disabled {
        return;
    }
    let filter = conditions::build_all(&rows);
    update_form(this, window, cx, |form| {
        form.trigger.filter = Some(filter);
        form.trigger.condition_rows = Some(rows);
    });
}

// --- schedule

const WEEKDAYS: [(&str, &str, &str); 7] = [
    ("1", "Sunday", "S"),
    ("2", "Monday", "M"),
    ("3", "Tuesday", "T"),
    ("4", "Wednesday", "W"),
    ("5", "Thursday", "T"),
    ("6", "Friday", "F"),
    ("7", "Saturday", "S"),
];

fn set_schedule<V: EditorHost>(
    this: &mut V,
    form: ScheduleForm,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let trigger = schedule::trigger_from_form(&form);
    update_form(this, window, cx, |workflow| workflow.trigger = trigger);
}

/// A pill (`.workflow-pill`): a card radio, or a round weekday toggle.
fn pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    round: bool,
    disabled: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let c = cx.colors();
    let (border, bg, text) = if disabled {
        (c.border_standard, c.affordance_disabled, c.text_unavailable)
    } else if checked {
        (
            c.affordance_prominent,
            c.affordance_selected,
            c.text_standard,
        )
    } else {
        (c.border_standard, c.surface_panel, c.text_subtle)
    };
    let hover_border = c.border_prominent;
    let hover_text = c.text_standard;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .min_h(rems(2.5))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(text)
        .text_body()
        .map(|this| {
            if round {
                this.size(rems(2.5)).rounded_full()
            } else {
                this.px(space::S3).py(space::S2).rounded(radius::CONTROL)
            }
        })
        .when(!disabled, |this| {
            this.cursor_pointer()
                .when(!checked, |this| {
                    this.hover(move |s| s.border_color(hover_border).text_color(hover_text))
                })
                .on_click(move |_, window, cx| on_click(window, cx))
        })
        .when(disabled, |this| this.cursor_not_allowed())
        .child(label.into())
}

fn schedule_fields<V: EditorHost>(
    this: &mut V,
    trigger: &TriggerConfig,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Div {
    let c = *cx.colors();
    let force_custom =
        this.editor_ref().and_then(|e| e.force_custom_cron) == Some(TriggerType::Schedule);
    let parsed = schedule::form_from_trigger(trigger);
    let form = if force_custom {
        ScheduleForm {
            custom_cron: trigger.cron.clone().unwrap_or(parsed.custom_cron.clone()),
            frequency: Frequency::CustomCron,
            ..parsed
        }
    } else {
        parsed
    };
    let menu = this.editor_ref().and_then(|e| e.menu.clone());
    // Repeats
    let mut grid = div().grid().grid_cols(2).gap(space::S2);
    for frequency in FREQUENCIES {
        let checked = form.frequency == frequency;
        let current = form.clone();
        let host = cx.entity().downgrade();
        let item = pill(
            SharedString::from(format!("workflow-repeat-{frequency:?}")),
            frequency.label(),
            checked,
            false,
            disabled,
            move |window, cx| {
                let current = current.clone();
                let _ = host.update(cx, |this, cx| {
                    let custom = frequency == Frequency::CustomCron;
                    if let Some(editor) = this.editor() {
                        editor.force_custom_cron = custom.then_some(TriggerType::Schedule);
                    }
                    let next = ScheduleForm {
                        custom_cron: if custom {
                            schedule::custom_cron_seed(&current)
                        } else {
                            current.custom_cron.clone()
                        },
                        frequency,
                        ..current
                    };
                    set_schedule(this, next, window, cx);
                });
            },
            cx,
        );
        grid = grid.child(if frequency == Frequency::CustomCron {
            div().col_span(2).child(item)
        } else {
            div().child(item)
        });
    }
    let mut out = div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .child(controls::field("Repeats", None, None, grid, cx));
    if form.frequency == Frequency::Weekly {
        let selected: HashSet<String> = schedule::weekdays_from_field(&form.weekday)
            .into_iter()
            .collect();
        let mut days = div().flex().gap(space::S2);
        for (value, label, short) in WEEKDAYS {
            let checked = selected.contains(value);
            let current = form.clone();
            let selected = selected.clone();
            let host = cx.entity().downgrade();
            days = days.child(pill(
                SharedString::from(format!("workflow-weekday-{value}")),
                short,
                checked,
                true,
                disabled,
                move |window, cx| {
                    let mut next = selected.clone();
                    if next.contains(value) {
                        // A weekly schedule needs at least one day.
                        if next.len() == 1 {
                            return;
                        }
                        next.remove(value);
                    } else {
                        next.insert(value.to_string());
                    }
                    let weekday = WEEKDAYS
                        .iter()
                        .map(|(day, _, _)| *day)
                        .filter(|day| next.contains(*day))
                        .collect::<Vec<_>>()
                        .join(",");
                    let current = current.clone();
                    let _ = host.update(cx, |this, cx| {
                        set_schedule(this, ScheduleForm { weekday, ..current }, window, cx)
                    });
                },
                cx,
            ));
            let _ = label;
        }
        out = out.child(controls::field("Repeat on", None, None, days, cx));
    }
    if form.frequency == Frequency::Monthly {
        let current = form.clone();
        let select = controls::select_field(
            "workflow-month-day",
            "Day of month",
            Some(form.month_day.clone().into()),
            (1..=31)
                .map(|day| {
                    (
                        SharedString::from(day.to_string()),
                        form.month_day == day.to_string(),
                    )
                })
                .collect(),
            menu == Some(OpenMenu::MonthDay),
            disabled,
            |this: &mut V, cx| toggle_menu(this, OpenMenu::MonthDay, cx),
            move |this: &mut V, picked, window, cx| {
                set_menu(this, None, cx);
                let month_day = (picked + 1).to_string();
                set_schedule(
                    this,
                    ScheduleForm {
                        month_day,
                        ..current.clone()
                    },
                    window,
                    cx,
                );
            },
            dismiss(cx),
            window,
            cx,
        );
        out = out.child(
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .child(select)
                .children(schedule::month_day_warning(&form.month_day).map(|warning| {
                    div()
                        .text_body_sm()
                        .text_color(c.text_warning)
                        .child(warning)
                })),
        );
    }
    if form.frequency.uses_time() {
        let current = form.clone();
        let state = line(
            this,
            "schedule:time".into(),
            &form.time,
            "09:00",
            window,
            cx,
            move |this, value, window, cx| {
                set_schedule(
                    this,
                    ScheduleForm {
                        time: value,
                        ..current.clone()
                    },
                    window,
                    cx,
                );
            },
        );
        out = out.child(controls::field(
            "Run time (UTC)",
            None,
            None,
            text_field(&state, false, disabled),
            cx,
        ));
    }
    if form.frequency == Frequency::CustomCron {
        out = out.child(cron_input(
            this,
            &form.custom_cron,
            form.clone(),
            disabled,
            window,
            cx,
        ));
    }
    if form.frequency == Frequency::CustomInterval {
        let current = form.clone();
        let state = line(
            this,
            "schedule:interval".into(),
            &form.custom_interval,
            "",
            window,
            cx,
            move |this, value, window, cx| {
                set_schedule(
                    this,
                    ScheduleForm {
                        custom_interval: value,
                        ..current.clone()
                    },
                    window,
                    cx,
                );
            },
        );
        out = out.child(
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .child(div().text_label_sm().child("Existing interval"))
                .child(text_field(&state, false, disabled))
                .child(controls::note(
                    "Keep this legacy interval or choose a repeat option above. All schedules use UTC.",
                    cx,
                ))
                .children(schedule::interval_warning(&form.custom_interval).map(|warning| {
                    div().text_body_sm().text_color(c.text_warning).child(warning)
                })),
        );
    }
    out
}

/// Five boxes for one five-field cron expression
/// (`CronExpressionInput.tsx`). A space hops to the next box; text with
/// spaces, as a pasted expression, fills all five.
fn cron_input<V: EditorHost>(
    this: &mut V,
    value: &str,
    current: ScheduleForm,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Div {
    let c = *cx.colors();
    // An outside change (YAML, a preset seed) replaces the boxes; our own
    // commits do not, so an emptied box stays empty.
    if let Some(editor) = this.editor() {
        let normalized = cron::normalize(value);
        if normalized != editor.cron_local
            || editor.cron_fields.iter().all(String::is_empty) && !normalized.is_empty()
        {
            editor.cron_fields = cron::fields_from_expression(value);
            editor.cron_local = normalized;
            editor.cron_error = None;
        }
    }
    let editor = this.editor_ref().expect("editor");
    let fields = editor.cron_fields.clone();
    let paste_error = editor.cron_error.clone();
    let errors = cron::field_errors(&fields);
    let first_error = paste_error.or_else(|| errors.iter().flatten().next().cloned());
    let mut boxes = div().grid().grid_cols(5).gap(space::S2);
    let mut states = Vec::new();
    for index in 0..5 {
        let definition = &cron::FIELDS[index];
        let current = current.clone();
        let state = line(
            this,
            format!("cron:{index}"),
            &fields[index],
            "*",
            window,
            cx,
            move |this, typed, window, cx| {
                cron_typed(this, index, typed, current.clone(), window, cx);
            },
        );
        states.push(state.clone());
        let error = errors[index].is_some();
        boxes = boxes.child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(space::S1)
                .min_w_0()
                .child(
                    div()
                        .text_body_sm()
                        .text_color(if error { c.text_danger } else { c.text_subtle })
                        .child(definition.label),
                )
                .child(
                    div()
                        .w_full()
                        .text_mono()
                        .on_key_down(cx.listener(
                            move |this: &mut V, event: &KeyDownEvent, window, cx| {
                                if event.keystroke.key == "backspace"
                                    && index > 0
                                    && this
                                        .editor_ref()
                                        .is_some_and(|e| e.cron_fields[index].is_empty())
                                {
                                    focus_cron(this, index - 1, window, cx);
                                }
                            },
                        ))
                        .child(text_field(&state, error, disabled)),
                ),
        );
    }
    let _ = states;
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .child(div().text_label_sm().child("Cron expression"))
        .child(boxes)
        .child(
            div()
                .text_body_sm()
                .text_color(if first_error.is_some() {
                    c.text_danger
                } else {
                    c.text_subtle
                })
                .child(first_error.unwrap_or_else(|| {
                    "UTC · Paste all 5 fields, or use wildcards, lists, ranges, and steps.".into()
                })),
        )
}

fn focus_cron<V: EditorHost>(this: &mut V, index: usize, window: &mut Window, cx: &mut Context<V>) {
    let state = this.editor_ref().and_then(|editor| {
        match editor.inputs.slots.get(&format!("cron:{index}")) {
            Some((Slot::Line(state), ..)) => Some(state.clone()),
            _ => None,
        }
    });
    if let Some(state) = state {
        let handle = state.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        state.update(cx, |state, cx| state.select_all(window, cx));
    }
}

fn cron_typed<V: EditorHost>(
    this: &mut V,
    index: usize,
    typed: String,
    current: ScheduleForm,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let Some(editor) = this.editor() else { return };
    let mut fields = editor.cron_fields.clone();
    let tokens: Vec<&str> = typed.split_whitespace().collect();
    let hop = typed.contains(char::is_whitespace);
    if tokens.len() > 1 {
        // A whole expression pasted into one box.
        match cron::fields_from_paste(&typed) {
            Ok(pasted) => fields = pasted,
            Err(error) => {
                editor.cron_error = Some(error);
                fields[index] = typed.chars().filter(|c| !c.is_whitespace()).collect();
                let state = match editor.inputs.slots.get(&format!("cron:{index}")) {
                    Some((Slot::Line(state), ..)) => Some(state.clone()),
                    _ => None,
                };
                editor.cron_fields = fields;
                if let Some(state) = state {
                    let value = editor.cron_fields[index].clone();
                    state.update(cx, |state, cx| state.set_value(value, window, cx));
                }
                cx.notify();
                return;
            }
        }
    } else {
        fields[index] = typed.chars().filter(|c| !c.is_whitespace()).collect();
    }
    let expression = cron::expression_from_fields(&fields);
    editor.cron_fields = fields.clone();
    editor.cron_local = cron::normalize(&expression);
    editor.cron_error = None;
    // Keep each box showing its field, without the typed space.
    let states: Vec<(usize, Entity<InputState>)> = (0..5)
        .filter_map(|i| match editor.inputs.slots.get(&format!("cron:{i}")) {
            Some((Slot::Line(state), ..)) => Some((i, state.clone())),
            _ => None,
        })
        .collect();
    for (i, state) in states {
        if state.read(cx).value() != fields[i] {
            let value = fields[i].clone();
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }
    }
    set_schedule(
        this,
        ScheduleForm {
            custom_cron: expression,
            ..current
        },
        window,
        cx,
    );
    if hop && tokens.len() <= 1 && index < 4 {
        focus_cron(this, index + 1, window, cx);
    }
}

// --- steps

#[allow(clippy::too_many_arguments)]
fn step_fields<V: EditorHost>(
    this: &mut V,
    form: &WorkflowForm,
    step: &Step,
    index: usize,
    issue: Option<&Issue>,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Div {
    let c = *cx.colors();
    let error = |field: IssueField| {
        issue
            .filter(|issue| issue.step == Some(index) && issue.field == Some(field))
            .map(|issue| SharedString::from(issue.message))
    };
    let id = step.id.clone();
    let key = |field: &str| format!("step:{}:{field}", step.id);
    let mut out = div().flex().flex_col().gap(space::S4);
    match step.action {
        ActionType::CallWebhook => {
            out = out.child(webhook_fields(
                this,
                step,
                disabled,
                error(IssueField::Url),
                error(IssueField::Headers),
                window,
                cx,
            ));
        }
        ActionType::SendMessage => {
            let id = id.clone();
            let state = area(
                this,
                key("text"),
                step.text.as_deref().unwrap_or(""),
                window,
                cx,
                move |this, value, window, cx| {
                    update_step(this, &id, window, cx, |step| step.text = Some(value));
                },
            );
            out = out.child(controls::field(
                "Message text",
                Some(
                    "Messages post to the workflow channel unless a destination is set below."
                        .into(),
                ),
                error(IssueField::Text),
                TextArea::new(&state)
                    .rows(4)
                    .surface(Surface::Floating)
                    .invalid(error(IssueField::Text).is_some())
                    .disabled(disabled),
                cx,
            ));
        }
        ActionType::Delay => {
            let id = id.clone();
            let state = line(
                this,
                key("duration"),
                step.duration.as_deref().unwrap_or(""),
                "5m",
                window,
                cx,
                move |this, value, window, cx| {
                    update_step(this, &id, window, cx, |step| step.duration = Some(value));
                },
            );
            out = out.child(controls::field(
                "Delay duration",
                None,
                error(IssueField::Duration),
                text_field(&state, error(IssueField::Duration).is_some(), disabled),
                cx,
            ));
        }
    }
    // Run controls; a timeout error opens them.
    let timeout_error = error(IssueField::Timeout);
    if timeout_error.is_some()
        && let Some(editor) = this.editor()
    {
        editor.expanded.insert((step.id.clone(), "run"));
    }
    let expanded = this
        .editor_ref()
        .map(|e| e.expanded.clone())
        .unwrap_or_default();
    let run_open = expanded.contains(&(step.id.clone(), "run"));
    let details_open = expanded.contains(&(step.id.clone(), "details"));
    let caption = if step.timeout_secs.as_deref().is_some_and(|t| !t.is_empty())
        || step.reply_in_thread == Some(true)
        || step.condition.as_deref().is_some_and(|t| !t.is_empty())
    {
        [
            step.condition
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|_| "Conditional".to_string()),
            (step.reply_in_thread == Some(true)).then(|| "Thread reply".to_string()),
            step.timeout_secs.clone().filter(|t| !t.is_empty()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
    } else {
        "Default".into()
    };
    let toggle = |section: &'static str, cx: &mut Context<V>| {
        let host = cx.entity().downgrade();
        let id = step.id.clone();
        move |_: &mut Window, cx: &mut App| {
            let id = id.clone();
            let _ = host.update(cx, |this, cx| {
                if let Some(editor) = this.editor() {
                    let key = (id, section);
                    if !editor.expanded.remove(&key) {
                        editor.expanded.insert(key);
                    }
                }
                cx.notify();
            });
        }
    };
    out = out.child(controls::accordion_trigger(
        SharedString::from(format!("workflow-run-controls-{}", step.id)),
        "Run controls",
        Some(caption.into()),
        run_open,
        toggle("run", cx),
        cx,
    ));
    if run_open {
        let mut run = div().flex().flex_col().gap(space::S4).p(space::S2);
        if step.action == ActionType::SendMessage && form.trigger.on.thread_reply_eligible() {
            let id = id.clone();
            run = run.child(
                Switch::new(
                    SharedString::from(format!("workflow-thread-{}", step.id)),
                    step.reply_in_thread == Some(true),
                )
                .label("Reply in the triggering thread")
                .disabled(disabled)
                .on_change({
                    let host = cx.entity().downgrade();
                    move |value, window, cx| {
                        let id = id.clone();
                        let _ = host.update(cx, |this, cx| {
                            update_step(this, &id, window, cx, |step| {
                                step.reply_in_thread = Some(value)
                            });
                        });
                    }
                }),
            );
        }
        let condition_id = id.clone();
        let condition = line(
            this,
            key("condition"),
            step.condition.as_deref().unwrap_or(""),
            "",
            window,
            cx,
            move |this, value, window, cx| {
                update_step(this, &condition_id, window, cx, |step| {
                    step.condition = (!value.trim().is_empty()).then_some(value);
                });
            },
        );
        run = run.child(controls::field(
            "Step condition (optional)",
            Some("An evalexpr expression evaluated before this step.".into()),
            None,
            text_field(&condition, false, disabled),
            cx,
        ));
        let timeout_id = id.clone();
        let timeout = line(
            this,
            key("timeout"),
            step.timeout_secs.as_deref().unwrap_or(""),
            "30s",
            window,
            cx,
            move |this, value, window, cx| {
                update_step(this, &timeout_id, window, cx, |step| {
                    step.timeout_secs = Some(value)
                });
            },
        );
        run = run.child(controls::field(
            "Step timeout (optional)",
            None,
            timeout_error.clone(),
            text_field(&timeout, timeout_error.is_some(), disabled),
            cx,
        ));
        out = out.child(run);
    }
    out = out.child(controls::accordion_trigger(
        SharedString::from(format!("workflow-step-details-{}", step.id)),
        "Step details",
        Some(
            step.name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| step.id.clone())
                .into(),
        ),
        details_open,
        toggle("details", cx),
        cx,
    ));
    if details_open {
        let mut details = div().flex().flex_col().gap(space::S4).p(space::S2).child(
            div()
                .text_mono()
                .text_color(c.text_subtle)
                .child(step.id.clone()),
        );
        let name_id = id.clone();
        let name = line(
            this,
            key("name"),
            step.name.as_deref().unwrap_or(""),
            "",
            window,
            cx,
            move |this, value, window, cx| {
                update_step(this, &name_id, window, cx, |step| step.name = Some(value));
            },
        );
        details = details.child(controls::field(
            "Step name (optional)",
            None,
            None,
            text_field(&name, false, disabled),
            cx,
        ));
        if step.action == ActionType::SendMessage {
            let channel_id = id.clone();
            let channel = line(
                this,
                key("channel"),
                step.channel.as_deref().unwrap_or(""),
                "",
                window,
                cx,
                move |this, value, window, cx| {
                    update_step(this, &channel_id, window, cx, |step| {
                        step.channel = Some(value)
                    });
                },
            );
            details = details.child(controls::field(
                "Destination channel UUID (optional)",
                Some(
                    "Blank uses this workflow’s channel. The relay checks destination access."
                        .into(),
                ),
                None,
                text_field(&channel, false, disabled),
                cx,
            ));
        }
        out = out.child(details);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn webhook_fields<V: EditorHost>(
    this: &mut V,
    step: &Step,
    disabled: bool,
    url_error: Option<SharedString>,
    headers_error: Option<SharedString>,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Div {
    let id = step.id.clone();
    let key = |field: &str| format!("step:{}:{field}", step.id);
    let menu = this.editor_ref().and_then(|e| e.menu.clone());
    let url_id = id.clone();
    let url = line(
        this,
        key("url"),
        step.url.as_deref().unwrap_or(""),
        "https://example.com/hook",
        window,
        cx,
        move |this, value, window, cx| {
            update_step(this, &url_id, window, cx, |step| step.url = Some(value));
        },
    );
    let methods = [
        "", "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS",
    ];
    let method = step.method.clone().unwrap_or_default();
    let method_id = id.clone();
    let mut out = div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .child(controls::field("Webhook URL", None, url_error.clone(), text_field(&url, url_error.is_some(), disabled), cx))
        .child(controls::select_field(
            "workflow-method",
            "HTTP method",
            Some(if method.is_empty() { "Relay default".into() } else { method.clone().into() }),
            methods
                .iter()
                .map(|m| (SharedString::from(if m.is_empty() { "Relay default" } else { m }), *m == method))
                .collect(),
            menu == Some(OpenMenu::Method),
            disabled,
            |this: &mut V, cx| toggle_menu(this, OpenMenu::Method, cx),
            move |this: &mut V, picked, window, cx| {
                set_menu(this, None, cx);
                let value = methods[picked].to_string();
                update_step(this, &method_id, window, cx, |step| step.method = Some(value));
            },
            dismiss(cx),
            window,
            cx,
        ))
        .child(controls::note(
            "The relay checks permission and destination safety. Webhook steps require a channel owner or admin. Configuration, including headers, is visible to channel members. Do not put secrets here.",
            cx,
        ));
    let headers = step.headers.clone().unwrap_or_default();
    let mut list = div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .child(div().text_label_sm().child("Headers"));
    for (index, header) in headers.iter().enumerate() {
        let name_id = id.clone();
        let name = line(
            this,
            key(&format!("header:{}:name", header.id)),
            &header.name,
            "",
            window,
            cx,
            move |this, value, window, cx| {
                update_step(this, &name_id, window, cx, |step| {
                    if let Some(header) = step.headers.as_mut().and_then(|h| h.get_mut(index)) {
                        header.name = value;
                    }
                });
            },
        );
        let value_id = id.clone();
        let value = line(
            this,
            key(&format!("header:{}:value", header.id)),
            &header.value,
            "",
            window,
            cx,
            move |this, value, window, cx| {
                update_step(this, &value_id, window, cx, |step| {
                    if let Some(header) = step.headers.as_mut().and_then(|h| h.get_mut(index)) {
                        header.value = value;
                    }
                });
            },
        );
        let remove_id = id.clone();
        list = list.child(
            div()
                .flex()
                .items_end()
                .gap(space::S2)
                .child(div().flex_1().min_w_0().child(controls::field(
                    format!("Header {} name", index + 1),
                    None,
                    None,
                    text_field(&name, false, disabled),
                    cx,
                )))
                .child(div().flex_1().min_w_0().child(controls::field(
                    format!("Header {} value", index + 1),
                    None,
                    None,
                    text_field(&value, false, disabled),
                    cx,
                )))
                .child(
                    IconButton::new(
                        SharedString::from(format!("workflow-header-remove-{}", header.id)),
                        IconName::Trash,
                    )
                    .size(IconButtonSize::Sm)
                    .surface(Surface::Floating)
                    .tooltip(format!("Remove header {}", index + 1))
                    .disabled(disabled)
                    .on_click(cx.listener(
                        move |this: &mut V, _, window, cx| {
                            update_step(this, &remove_id, window, cx, |step| {
                                if let Some(headers) = step.headers.as_mut() {
                                    headers.remove(index);
                                }
                            });
                        },
                    )),
                ),
        );
    }
    if let Some(error) = headers_error {
        list = list.child(controls::danger(error, cx));
    }
    let add_id = id.clone();
    list = list.child(
        div().flex().child(
            Button::new("workflow-header-add", "Add header")
                .size(ButtonSize::Sm)
                .surface(Surface::Floating)
                .disabled(disabled)
                .on_click(cx.listener(move |this: &mut V, _, window, cx| {
                    update_step(this, &add_id, window, cx, |step| {
                        step.headers.get_or_insert_with(Vec::new).push(Header {
                            id: uuid::Uuid::new_v4().to_string(),
                            name: String::new(),
                            value: String::new(),
                        });
                    });
                })),
        ),
    );
    out = out.child(list);
    let body_id = id.clone();
    let body = area(
        this,
        key("body"),
        step.body.as_deref().unwrap_or(""),
        window,
        cx,
        move |this, value, window, cx| {
            update_step(this, &body_id, window, cx, |step| step.body = Some(value));
        },
    );
    out.child(controls::field(
        "Request body (optional)",
        None,
        None,
        TextArea::new(&body)
            .code()
            .rows(4)
            .surface(Surface::Floating)
            .disabled(disabled),
        cx,
    ))
}
