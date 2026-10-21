//! "Project settings" (`sessions/WorkspaceSettings.tsx`,
//! `DraftSessionSettings.tsx`, `WorkspaceFields.tsx`): project folders,
//! a new worktree and Canvas for a session, a Me group's defaults, or a
//! new conversation's draft.
//!
//! A session's settings are its Canvas; a group's are its own template
//! record (`me-section-<sha256>`), which new conversations in the group
//! start from. Both save with the revision the dialog loaded, and the
//! edit is kept on this device until the save lands, so a conflict or a
//! failure never loses it. A new conversation's settings stay on this
//! device until its first send applies them.

use super::dialogs::DialogEvent;
use super::pending::{self, SessionSetup};
use super::workspace::{
    self, Lineup, Workspace, Worktree, read_workspace, section_template, workspace_canvas,
};
use crate::community::{AppState, Community};
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::Session;
use buzz_relay_client::kit::me::{self as kit, GroupsId, REMOVED_SECTION, RawKitEntry as KitEntry};
use buzz_relay_client::session::AgentRules;
use buzz_relay_client::sidebar_store::PreferencesStatus;
use buzz_ui::{
    ActiveTheme, Button, ButtonShape, Dialog, Menu, MenuItem, Select, Surface, Switch, TextArea,
    TextField, TypeStyled,
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::rc::Rc;
use std::sync::Arc;

/// The template record holding a group's session defaults
/// (`sectionTemplateId`): `me-section-<sha256 of the group id>`.
pub fn section_template_id(section: &str, personal: bool) -> String {
    let hash = Sha256::digest(section.as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-section-{hex}", if personal { "me" } else { "session" })
}

/// A group's defaults (`sectionDefault`): its own template record, else
/// the group's default template. `require` refuses a default that names
/// a missing template.
pub fn section_default(
    entries: &[KitEntry],
    section: &str,
    personal: bool,
    require: bool,
) -> Result<Option<serde_json::Value>, String> {
    if let Some(own) = kit::template(entries, &section_template_id(section, personal)) {
        return Ok(Some(own.record["value"].clone()));
    }
    // Only Me groups carry defaults since personal groups were retired.
    let groups = personal
        .then(|| kit::groups(entries, GroupsId::Me))
        .flatten();
    let default = groups.and_then(|groups| {
        groups
            .groups
            .into_iter()
            .find(|group| group.id == section)
            .map(|group| group.default_template_id)
            .filter(|id| !id.is_empty())
    });
    let Some(default) = default else {
        return Ok(None);
    };
    match kit::template(entries, &default) {
        Some(template) => Ok(Some(template.record["value"].clone())),
        None if require => Err("This section’s default template is unavailable. Choose new section defaults before starting a session.".into()),
        None => Ok(None),
    }
}

/// The defaults a new conversation in `section` starts with
/// (`loadSessionSetup`).
pub async fn load_session_setup(
    session: &Session,
    section: &str,
    personal: bool,
) -> Result<SessionSetup, String> {
    let kit = session.me_kit();
    kit.me_refresh();
    let mut state = kit.watch_me();
    let _ = state
        .wait_for(|state| state.status != PreferencesStatus::Loading)
        .await;
    let me = kit.me_state();
    if !me.writable
        || !me
            .data
            .as_ref()
            .is_some_and(|data| data.sections.iter().any(|s| s.id == section))
    {
        return Err(
            "This section is unavailable. Refresh sections before starting a session.".into(),
        );
    }
    kit.refresh().await;
    let snapshot = kit.snapshot();
    if !snapshot.ready() {
        return Err(snapshot
            .error()
            .unwrap_or("Section defaults couldn’t load. Retry before starting the session.")
            .to_owned());
    }
    let Some(template) = section_default(&snapshot.entries, section, personal, true)? else {
        return Ok(SessionSetup {
            section_id: Some(section.to_owned()),
            canvas: String::new(),
            agents: Vec::new(),
        });
    };
    let mut agents: Vec<String> = template["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| key.as_str().map(str::to_owned))
        .collect();
    for team in template["teamIds"].as_array().into_iter().flatten() {
        let team = team.as_str().unwrap_or_default();
        let entry = snapshot
            .entries
            .iter()
            .find(|e| e.kind() == "team" && e.id() == team && !e.deleted())
            .ok_or_else(|| {
                format!("A selected team is unavailable ({team}); remove or replace it")
            })?;
        for key in entry.record["value"]["agents"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(key) = key.as_str()
                && !agents.iter().any(|known| known == key)
            {
                agents.push(key.to_owned());
            }
        }
    }
    Ok(SessionSetup {
        section_id: Some(section.to_owned()),
        canvas: template["canvas"].as_str().unwrap_or_default().to_owned(),
        agents,
    })
}

/// Applies a new conversation's defaults once it exists
/// (`applySessionSetup`): its Canvas, its group, its agents.
pub async fn apply_session_setup(
    session: &Session,
    channel: &str,
    setup: &SessionSetup,
    personal: bool,
    rules: &AgentRules,
) -> Result<(), String> {
    let kit = session.me_kit();
    let head = kit.canvas_read(channel).await?;
    match head {
        Some(head) if head.event().content != setup.canvas => {
            return Err(
                "This session already has a different Canvas. Review it before retrying setup."
                    .into(),
            );
        }
        None if !setup.canvas.is_empty() => {
            kit.canvas_save(channel, &setup.canvas, None).await?;
        }
        _ => {}
    }
    if let Some(section) = &setup.section_id
        && personal
    {
        kit.me_refresh();
        let mut state = kit.watch_me();
        let _ = state
            .wait_for(|state| state.status != PreferencesStatus::Loading)
            .await;
        let me = kit.me_state();
        let Some(data) = me
            .data
            .as_ref()
            .filter(|_| me.status == PreferencesStatus::Ready)
        else {
            return Err("Sections couldn’t load. Retry before continuing setup.".into());
        };
        if !data.sections.iter().any(|s| s.id == *section) {
            return Err(REMOVED_SECTION.into());
        }
        if data.assignments.get(channel) != Some(section) {
            kit.me_assign(channel, Some(section.clone())).await?;
        }
    }
    if !setup.agents.is_empty() {
        session.add_agents(channel, &setup.agents, rules).await?;
    }
    Ok(())
}

/// What the settings are for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsTarget {
    /// A conversation's Canvas.
    Session { id: String, name: String },
    /// A Me group's defaults for new conversations.
    Section { id: String, name: String },
    /// A new conversation before it starts: saved on this device under
    /// its draft key, applied by the first send.
    Draft {
        key: String,
        section: Option<String>,
    },
}

impl SettingsTarget {
    /// Me's groups are personal; a conversation's Canvas is not.
    fn personal(&self) -> bool {
        matches!(self, Self::Section { .. })
    }

    /// Where an unsaved edit is kept on this device.
    fn draft_key(&self) -> String {
        let (kind, id) = match self {
            Self::Session { id, .. } => ("session", id),
            Self::Section { id, .. } => ("section", id),
            Self::Draft { key, .. } => return format!("{key}:settings"),
        };
        let scope = if self.personal() { "me" } else { "sessions" };
        format!("{scope}:workspace-draft:{kind}:{id}")
    }
}

/// The settings as saved on the relay.
#[derive(Clone, Debug, Default)]
pub struct Saved {
    pub content: String,
    /// The Canvas head or the template record the save must replace.
    pub revision: Option<String>,
    /// A group's template record id, the catalog it starts from, and its
    /// current agents and teams.
    pub recipe_id: String,
    pub entries: Arc<[KitEntry]>,
    pub lineup: Lineup,
}

/// Reads a session's Canvas or a group's defaults.
pub async fn read_saved(session: &Session, target: &SettingsTarget) -> Result<Saved, String> {
    let kit = session.me_kit();
    match target {
        SettingsTarget::Section { id, .. } => {
            kit.refresh().await;
            let snapshot = kit.snapshot();
            if !snapshot.ready() {
                return Err(snapshot
                    .error()
                    .unwrap_or("Section defaults are unavailable.")
                    .to_owned());
            }
            let recipe_id = section_template_id(id, target.personal());
            // A deleted record still owns the revision, though it
            // supplies no defaults.
            let revision = snapshot
                .entries
                .iter()
                .find(|entry| entry.kind() == "template" && entry.id() == recipe_id)
                .map(|entry| entry.event_id.clone());
            let lineup = section_default(&snapshot.entries, id, target.personal(), false)?
                .map(|template| Lineup::parse(&template))
                .transpose()?
                .unwrap_or_default();
            Ok(Saved {
                content: lineup.canvas.clone(),
                revision,
                recipe_id,
                entries: snapshot.entries.clone(),
                lineup,
            })
        }
        SettingsTarget::Session { id, .. } => {
            let head = kit.canvas_read(id).await?;
            Ok(Saved {
                content: head
                    .as_ref()
                    .map(|head| head.event().content.clone())
                    .unwrap_or_default(),
                revision: head.map(|head| head.id_hex()),
                ..Saved::default()
            })
        }
        SettingsTarget::Draft { .. } => Ok(Saved::default()),
    }
}

/// Saves a session's Canvas, or a group's defaults as its template
/// record, over the revision the edit started from.
pub async fn write_saved(
    session: &Session,
    target: &SettingsTarget,
    canvas: &str,
    base: Option<String>,
    recipe_id: &str,
    lineup: &Lineup,
) -> Result<(), String> {
    let kit = session.me_kit();
    match target {
        SettingsTarget::Section { name, .. } => {
            let template = section_template(recipe_id, name, lineup, canvas);
            // The whole encrypted record's budget, before publishing.
            workspace::check_template(&template)?;
            kit.save(template, base, false).await.map(|_| ())
        }
        SettingsTarget::Session { id, .. } => kit.canvas_save(id, canvas, base).await.map(|_| ()),
        SettingsTarget::Draft { .. } => Ok(()),
    }
}

/// An unsaved edit as kept on this device: `{canvas, base, lineup}`.
fn local_draft(value: Option<Value>) -> Option<(String, Option<String>, Option<Value>)> {
    let value = value?;
    let canvas = value.get("canvas")?.as_str()?.to_owned();
    let base = match value.get("base")? {
        Value::Null => None,
        Value::String(base) => Some(base.clone()),
        _ => return None,
    };
    Some((canvas, base, value.get("lineup").cloned()))
}

const STALE: &str = "Saved settings changed while this draft was open. Your draft is kept; copy it before reloading.";
const NOT_KEPT: &str = "Your draft couldn’t be saved on this device. No changes were sent.";
const FOLDER_PLACEHOLDER: &str = "~/Development/my-project";
const CANVAS_PLACEHOLDER: &str = "Instructions and context for agents working in this session.";
/// The dialog's content width: 496 less its padding and border.
const CONTENT_WIDTH: f32 = 446.;

pub struct WorkspaceSettings {
    community: Entity<Community>,
    target: SettingsTarget,
    focus: FocusHandle,
    /// The fields' value; the inputs below write into it.
    workspace: Workspace,
    /// The Canvas could not be read as a workspace: edit it whole.
    raw: bool,
    raw_input: Entity<TextareaState>,
    folders: Entity<TextareaState>,
    repositories: Vec<(Entity<InputState>, Subscription)>,
    location: Entity<InputState>,
    branch: Entity<InputState>,
    canvas: Entity<TextareaState>,
    /// The revision the edit replaces.
    base: Option<String>,
    recipe_id: String,
    lineup: Lineup,
    entries: Arc<[KitEntry]>,
    loaded: bool,
    busy: bool,
    error: Option<SharedString>,
    reload_confirmed: bool,
    selected_template: String,
    template_menu: bool,
    /// The repository slot a folder picker is open for.
    picking: Option<usize>,
    picker_error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for WorkspaceSettings {}

impl WorkspaceSettings {
    pub fn new(
        community: Entity<Community>,
        target: SettingsTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let folders = cx.new(|cx| TextareaState::new(window, cx).placeholder(FOLDER_PLACEHOLDER));
        let canvas = cx.new(|cx| TextareaState::new(window, cx).placeholder(CANVAS_PLACEHOLDER));
        let raw_input = cx.new(|cx| TextareaState::new(window, cx));
        let location = cx.new(|cx| InputState::new(window, cx));
        let branch = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe(&folders, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    this.workspace.folders = text.split('\n').map(str::to_owned).collect();
                    cx.notify();
                }
            }),
            cx.subscribe(&canvas, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.workspace.canvas = input.read(cx).value().to_string();
                }
            }),
            cx.subscribe(&location, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(worktree) = &mut this.workspace.worktree
                {
                    worktree.location = input.read(cx).value().to_string();
                }
            }),
            cx.subscribe(&branch, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(worktree) = &mut this.workspace.worktree
                {
                    worktree.base_branch = input.read(cx).value().to_string();
                }
            }),
        ];
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut this = Self {
            community,
            target,
            focus,
            workspace: Workspace::default(),
            raw: false,
            raw_input,
            folders,
            repositories: Vec::new(),
            location,
            branch,
            canvas,
            base: None,
            recipe_id: String::new(),
            lineup: Lineup::default(),
            entries: Arc::new([]),
            loaded: false,
            busy: false,
            error: None,
            reload_confirmed: false,
            selected_template: String::new(),
            template_menu: false,
            picking: None,
            picker_error: None,
            _subscriptions: subscriptions,
        };
        this.load(false, window, cx);
        this
    }

    /// A press outside closes the saved settings (not while saving); a
    /// new conversation's settings close only from the dialog.
    pub fn dismissible(&self) -> bool {
        !self.busy && !matches!(self.target, SettingsTarget::Draft { .. })
    }

    fn draft(&self) -> bool {
        matches!(self.target, SettingsTarget::Draft { .. })
    }

    // ----- Fields (`WorkspaceFields.tsx`) -----

    /// Replaces the fields' value and shows it in the inputs.
    fn set_workspace(&mut self, workspace: Workspace, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace = workspace;
        let folders = self.workspace.folders.join("\n");
        self.folders
            .update(cx, |input, cx| input.set_value(folders, window, cx));
        let canvas = self.workspace.canvas.clone();
        self.canvas
            .update(cx, |input, cx| input.set_value(canvas, window, cx));
        let worktree = self.workspace.worktree.clone().unwrap_or_default();
        self.location.update(cx, |input, cx| {
            input.set_value(worktree.location, window, cx)
        });
        self.branch.update(cx, |input, cx| {
            input.set_value(worktree.base_branch, window, cx)
        });
        let slots = self.workspace.folders.len().max(1);
        self.repositories.truncate(slots);
        while self.repositories.len() < slots {
            let index = self.repositories.len();
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(FOLDER_PLACEHOLDER));
            let subscription = cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    let mut folders = this.slots();
                    if let Some(slot) = folders.get_mut(index) {
                        *slot = value;
                    }
                    this.workspace.folders = folders;
                }
            });
            self.repositories.push((input, subscription));
        }
        for (index, (input, _)) in self.repositories.iter().enumerate() {
            let value = self
                .workspace
                .folders
                .get(index)
                .cloned()
                .unwrap_or_default();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        cx.notify();
    }

    /// The repository slots: the folders, or one empty slot.
    fn slots(&self) -> Vec<String> {
        if self.workspace.folders.is_empty() {
            vec![String::new()]
        } else {
            self.workspace.folders.clone()
        }
    }

    fn set_worktree(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut workspace = self.workspace.clone();
        workspace.worktree = on.then(Worktree::default);
        self.set_workspace(workspace, window, cx);
    }

    /// "Browse…": a native folder picker for one repository slot.
    fn browse(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.picking = Some(index);
        self.picker_error = None;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a source repository".into()),
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let picked = match paths.await {
                Ok(Ok(paths)) => Ok(paths
                    .and_then(|paths| paths.into_iter().next())
                    .map(|path| path.to_string_lossy().into_owned())),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Ok(None),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.picking = None;
                match picked {
                    Ok(Some(path)) if this.workspace.worktree.is_some() => {
                        let mut folders = this.slots();
                        if let Some(slot) = folders.get_mut(index) {
                            *slot = path;
                            let mut workspace = this.workspace.clone();
                            workspace.folders = folders;
                            this.set_workspace(workspace, window, cx);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => this.picker_error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ----- Loading and saving -----

    /// Shows a Canvas as fields, or whole when it is not a workspace.
    fn adopt(&mut self, canvas: String, window: &mut Window, cx: &mut Context<Self>) {
        match read_workspace(&canvas) {
            Ok(workspace) => {
                self.raw = false;
                self.set_workspace(workspace, window, cx);
            }
            Err(_) => {
                self.raw = true;
                self.raw_input
                    .update(cx, |input, cx| input.set_value(canvas, window, cx));
            }
        }
    }

    fn read_local(&self, cx: &App) -> Option<Value> {
        let community = self.community.read(cx);
        cx.global::<AppState>()
            .store
            .view_state(&community.partition(), &self.target.draft_key())
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
    }

    fn write_local(&self, value: Option<&Value>, cx: &App) -> bool {
        let community = self.community.read(cx);
        let json = value.map(Value::to_string);
        cx.global::<AppState>()
            .store
            .set_view_state(
                &community.partition(),
                &self.target.draft_key(),
                json.as_deref(),
            )
            .is_ok()
    }

    /// Loads the saved settings; `replace` drops the edit kept on this
    /// device ("Discard draft and reload saved settings").
    fn load(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let session = self.community.read(cx).session.clone();
        if let SettingsTarget::Draft { key, section } = &self.target {
            // Only this device's draft; the first send applies it.
            self.error = None;
            let canvas = pending::draft_canvas(&self.community, key, cx);
            let section = section.clone();
            cx.notify();
            cx.spawn_in(window, async move |this, cx| {
                let content = match (canvas, section) {
                    (Some(canvas), _) => Ok(canvas),
                    (None, Some(section)) => load_session_setup(&session, &section, true)
                        .await
                        .map(|setup| setup.canvas),
                    (None, None) => Ok(String::new()),
                };
                let _ = this.update_in(cx, |this, window, cx| {
                    match content {
                        Ok(content) => {
                            this.adopt(content, window, cx);
                            this.loaded = true;
                        }
                        Err(error) => this.error = Some(error.into()),
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let target = self.target.clone();
        cx.spawn_in(window, async move |this, cx| {
            let saved = read_saved(&session, &target).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match saved {
                    Ok(saved) => {
                        if let Err(error) = this.loaded(saved, replace, window, cx) {
                            this.error = Some(error.into());
                        }
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn loaded(
        &mut self,
        saved: Saved,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let section = matches!(self.target, SettingsTarget::Section { .. });
        if section {
            self.recipe_id = saved.recipe_id;
            self.entries = saved.entries;
            self.lineup = saved.lineup;
        }
        let local = if replace {
            None
        } else {
            local_draft(self.read_local(cx))
        };
        match local {
            Some((canvas, base, lineup)) => {
                if section && let Some(lineup) = lineup {
                    self.lineup = Lineup::parse(&lineup)?;
                }
                self.adopt(canvas, window, cx);
                if base != saved.revision {
                    self.error = Some(STALE.into());
                }
                self.base = base;
            }
            None => {
                self.adopt(saved.content, window, cx);
                self.base = saved.revision;
            }
        }
        self.loaded = true;
        self.reload_confirmed = false;
        if replace {
            self.write_local(None, cx);
        }
        Ok(())
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || !self.loaded {
            return;
        }
        if let SettingsTarget::Draft { key, .. } = &self.target {
            let canvas = if self.raw {
                workspace_canvas(&Workspace {
                    canvas: self.raw_input.read(cx).value().to_string(),
                    ..Workspace::default()
                })
            } else {
                workspace_canvas(&self.workspace)
            };
            match canvas.and_then(|canvas| {
                pending::set_draft_canvas(&self.community, key, Some(&canvas), cx)
            }) {
                Ok(()) => cx.emit(DialogEvent::Close),
                Err(error) => {
                    self.error = Some(error.into());
                    cx.notify();
                }
            }
            return;
        }
        self.error = None;
        let canvas = if self.raw {
            Ok(self.raw_input.read(cx).value().to_string())
        } else {
            workspace_canvas(&self.workspace)
        };
        let canvas = canvas.and_then(|canvas| {
            let draft = json!({
                "canvas": canvas,
                "base": self.base,
                "lineup": self.lineup.to_value(),
            });
            if self.write_local(Some(&draft), cx) {
                Ok(canvas)
            } else {
                Err(NOT_KEPT.to_owned())
            }
        });
        let canvas = match canvas {
            Ok(canvas) => canvas,
            Err(error) => {
                self.error = Some(error.into());
                return cx.notify();
            }
        };
        self.busy = true;
        cx.notify();
        let session = self.community.read(cx).session.clone();
        let target = self.target.clone();
        let (base, recipe_id, lineup) = (
            self.base.clone(),
            self.recipe_id.clone(),
            self.lineup.clone(),
        );
        cx.spawn_in(window, async move |this, cx| {
            let result = write_saved(&session, &target, &canvas, base, &recipe_id, &lineup).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.write_local(None, cx);
                        cx.emit(DialogEvent::Close);
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            cx.emit(DialogEvent::Close);
        }
    }

    fn pick_template(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.template_menu = false;
        let template = workspace::starting_templates(&self.entries)
            .into_iter()
            .find(|entry| entry.id() == id)
            .map(|entry| entry.record["value"].clone());
        if let Some(template) = template
            && let Ok(lineup) = Lineup::parse(&template)
        {
            let canvas = lineup.canvas.clone();
            self.lineup = lineup;
            self.adopt(canvas, window, cx);
        }
        if id.is_empty() {
            self.lineup = Lineup::default();
        }
        self.selected_template = id;
        cx.notify();
    }
}

// ----- Rendering -----

/// A labelled field (`Field`): label-sm, 8px above its control.
fn field(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(rems(0.5))
        .min_w_0()
        .child(div().text_label_sm().child(label))
        .child(control)
}

fn paragraph(text: impl Into<SharedString>) -> Div {
    div().text_body().child(text.into())
}

impl WorkspaceSettings {
    /// `WorkspaceFields`.
    fn fields(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let busy = self.busy;
        let worktree = self.workspace.worktree.is_some();
        let toggle = div()
            .id("me-settings-worktree")
            .flex()
            .items_center()
            .gap(rems(0.75))
            .text_body()
            .cursor_pointer()
            .child("Work in a new worktree")
            .child(Switch::new("me-settings-worktree-switch", worktree))
            .on_click(cx.listener(move |this, _, window, cx| {
                if !this.busy {
                    this.set_worktree(!worktree, window, cx);
                }
            }));
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(rems(0.5))
            .child(
                div()
                    .text_body()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Project folders"),
            )
            .child(toggle);
        let mut fields = div().flex().flex_col().gap(rems(1.)).child(header);
        if !worktree {
            fields = fields.child(
                TextArea::new(&self.folders)
                    .rows(2)
                    .surface(Surface::Floating)
                    .disabled(busy),
            );
        } else {
            let count = self.repositories.len();
            let picking = self.picking.is_some();
            let repositories = self
                .repositories
                .iter()
                .enumerate()
                .map(|(index, (input, _))| {
                    let label = if count == 1 {
                        "Source repository".to_owned()
                    } else {
                        format!("Source repository {}", index + 1)
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.5))
                        .min_w_0()
                        .child(div().text_label_sm().child(label))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(rems(0.5))
                                .child(
                                    div().flex_1().min_w_0().child(
                                        TextField::new(input)
                                            .surface(Surface::Floating)
                                            .disabled(busy),
                                    ),
                                )
                                .child(
                                    Button::new(
                                        SharedString::from(format!("me-settings-browse-{index}")),
                                        "Browse…",
                                    )
                                    .shape(ButtonShape::Control)
                                    .surface(Surface::Floating)
                                    .disabled(picking || busy)
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| this.browse(index, window, cx),
                                    )),
                                ),
                        )
                });
            // Side by side unless the window is narrow (40rem).
            let narrow = window.viewport_size().width < window.rem_size() * 40.;
            let worktree_fields = div()
                .flex()
                .gap(rems(0.75))
                .when(narrow, |this| this.flex_col())
                .child(
                    field(
                        "Worktree location",
                        TextField::new(&self.location)
                            .surface(Surface::Floating)
                            .disabled(busy),
                    )
                    .flex_1(),
                )
                .child(
                    field(
                        "Base branch",
                        TextField::new(&self.branch)
                            .surface(Surface::Floating)
                            .disabled(busy),
                    )
                    .flex_1(),
                );
            fields = fields.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .children(repositories)
                    .when_some(self.picker_error.clone(), |this, error| {
                        this.child(paragraph(error))
                    })
                    .child(worktree_fields),
            );
        }
        fields.child(field(
            "Canvas",
            TextArea::new(&self.canvas)
                .rows(5)
                .surface(Surface::Floating)
                .disabled(busy),
        ))
    }

    /// "Start from a template": a group's defaults can begin from any
    /// saved template.
    fn template_select(&self, window: &Window, cx: &mut Context<Self>) -> Option<Div> {
        let templates = workspace::starting_templates(&self.entries);
        if !matches!(self.target, SettingsTarget::Section { .. }) || templates.is_empty() {
            return None;
        }
        let name = |entry: &KitEntry| -> SharedString {
            entry.record["value"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
                .into()
        };
        let value = templates
            .iter()
            .find(|entry| entry.id() == self.selected_template)
            .map(|entry| name(entry))
            .unwrap_or_else(|| "Custom settings".into());
        let open = self.template_menu;
        let mut select = div().relative().w_full().child(
            Select::new("me-settings-template")
                .value(value)
                .open(open)
                .surface(Surface::Floating)
                .disabled(self.busy)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.template_menu = !this.template_menu;
                    cx.notify();
                })),
        );
        if open {
            let options = std::iter::once((String::new(), SharedString::from("Custom settings")))
                .chain(
                    templates
                        .iter()
                        .map(|entry| (entry.id().to_owned(), name(entry))),
                )
                .enumerate()
                .map(|(index, (id, label))| {
                    MenuItem::new(SharedString::from(format!("me-template-{index}")), label)
                        .checked(id == self.selected_template)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pick_template(id.clone(), window, cx)
                        }))
                });
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.template_menu = false;
                    cx.notify();
                });
            });
            select = select.child(overlay::dropdown(
                window,
                Align::Start,
                div()
                    .w(px(CONTENT_WIDTH))
                    .child(Menu::new().children(options)),
                dismiss,
            ));
        }
        Some(field("Start from a template", select))
    }

    /// `WorkspaceSettings`: a session's Canvas or a group's defaults.
    fn saved_body(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = *cx.colors();
        let section = matches!(self.target, SettingsTarget::Section { .. });
        let mut fields = div().flex().flex_col().gap(rems(1.));
        if self.loaded {
            fields = fields.children(self.template_select(window, cx));
            fields = if self.raw {
                fields.child(
                    field(
                        "Full Canvas",
                        TextArea::new(&self.raw_input)
                            .rows(12)
                            .surface(Surface::Floating)
                            .disabled(self.busy),
                    )
                    .child(div().text_body_sm().text_color(c.text_subtle).child(
                        "This Canvas has custom workspace instructions. Edit it directly to preserve them.",
                    )),
                )
            } else {
                fields.child(self.fields(window, cx))
            };
            if section && (!self.lineup.agents.is_empty() || !self.lineup.team_ids.is_empty()) {
                fields = fields.child(
                    paragraph(
                        "The selected template’s agents and teams are also included in new chats.",
                    )
                    .text_color(c.text_subtle),
                );
            }
            if self.error.is_some() {
                let label = if self.reload_confirmed {
                    "Discard draft and reload saved settings"
                } else {
                    "Review saved settings"
                };
                fields = fields.child(
                    Button::new("me-settings-reload", label)
                        .ghost()
                        .surface(Surface::Floating)
                        .full_width()
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.reload_confirmed {
                                this.load(true, window, cx);
                            } else {
                                this.reload_confirmed = true;
                                cx.notify();
                            }
                        })),
                );
            }
        } else if !self.busy {
            fields = fields.child(
                Button::new("me-settings-retry", "Retry settings")
                    .surface(Surface::Floating)
                    .full_width()
                    .on_click(cx.listener(|this, _, window, cx| this.load(false, window, cx))),
            );
        }
        fields = fields.when_some(self.error.clone(), |this, error| {
            this.child(paragraph(error))
        });
        let mut body = vec![fields.into_any_element()];
        if self.busy && !self.loaded {
            body.push(paragraph("Loading settings…").into_any_element());
        }
        body
    }

    /// `DraftSessionSettings`: a new conversation's Canvas.
    fn draft_body(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut body = Vec::new();
        if !self.loaded {
            body.push(paragraph("Loading settings…").into_any_element());
        } else if self.raw {
            body.push(
                field(
                    "Canvas",
                    TextArea::new(&self.raw_input)
                        .rows(12)
                        .surface(Surface::Floating),
                )
                .into_any_element(),
            );
        } else {
            body.push(self.fields(window, cx).into_any_element());
        }
        if let Some(error) = self.error.clone() {
            body.push(paragraph(error).into_any_element());
            if !self.loaded {
                body.push(
                    div()
                        .flex()
                        .child(
                            Button::new("me-settings-retry", "Retry settings")
                                .surface(Surface::Floating)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.load(false, window, cx)),
                                ),
                        )
                        .into_any_element(),
                );
            }
        }
        body
    }
}

impl Render for WorkspaceSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let draft = self.draft();
        let body = if draft {
            self.draft_body(window, cx)
        } else {
            self.saved_body(window, cx)
        };
        let save_label = if matches!(self.target, SettingsTarget::Section { .. }) {
            "Save defaults"
        } else {
            "Save changes"
        };
        let dialog = Dialog::new("me-session-settings", "Project settings")
            .stable_height()
            .on_close(cx.listener(|this, _, _, cx| this.close(cx)))
            .children(body)
            .footer(
                Button::new("me-settings-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
            .footer(
                Button::new("me-settings-save", save_label)
                    .prominent()
                    .loading(self.busy)
                    .disabled(!self.loaded)
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            );
        // Escape reaches the dialog while no field has focus.
        div()
            .track_focus(&self.focus)
            .key_context(buzz_ui::DIALOG_CONTEXT)
            .on_action(cx.listener(|this, _: &buzz_ui::Cancel, _, cx| this.close(cx)))
            .child(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn template_ids_hash_the_group_like_buzz_app() {
        // `crypto.subtle.digest("SHA-256", "work")` in hex.
        assert_eq!(
            section_template_id("work", true),
            "me-section-00e13ed7af55b27622f1d6eab5bec0147e68efe28dc2b12461117afa1a5ed40e"
        );
        assert_eq!(
            section_template_id("g1", false),
            "session-section-711430f6164e93803d93428bc1fab80f41e213bb197689307de8606d437c3038"
        );
    }

    fn entry(value: Value, deleted: bool, event_id: &str) -> KitEntry {
        KitEntry {
            record: json!({"version": 1, "community": "c", "deleted": deleted, "value": value}),
            event_id: event_id.into(),
            created_at: 0,
        }
    }

    fn template(id: &str, canvas: &str) -> Value {
        json!({"type": "template", "id": id, "name": id, "description": "",
               "teamIds": [], "agents": [], "canvas": canvas})
    }

    fn me_groups(default: &str) -> Value {
        json!({"type": "groups", "id": "me", "assignments": {}, "channels": [],
               "groups": [{"id": "g", "name": "G", "defaultTemplateId": default}]})
    }

    #[test]
    fn section_defaults_prefer_the_groups_own_record() {
        let own = section_template_id("g", true);
        let entries = vec![
            entry(me_groups("t"), false, "1"),
            entry(template("t", "fallback"), false, "2"),
            entry(template(&own, "own"), false, "3"),
        ];
        let value = section_default(&entries, "g", true, true).unwrap().unwrap();
        assert_eq!(value["canvas"], "own");
        // A deleted own record falls back to the group's default.
        let entries = vec![
            entry(me_groups("t"), false, "1"),
            entry(template("t", "fallback"), false, "2"),
            entry(template(&own, "own"), true, "3"),
        ];
        let value = section_default(&entries, "g", true, true).unwrap().unwrap();
        assert_eq!(value["canvas"], "fallback");
        // Messages groups never supply Me defaults.
        assert!(
            section_default(&entries, "g", false, true)
                .unwrap()
                .is_none()
        );
        // A missing default is an error only when required.
        let entries = vec![entry(me_groups("gone"), false, "1")];
        assert!(
            section_default(&entries, "g", true, false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            section_default(&entries, "g", true, true).unwrap_err(),
            "This section’s default template is unavailable. Choose new section defaults before starting a session."
        );
        let entries = vec![entry(me_groups(""), false, "1")];
        assert!(
            section_default(&entries, "g", true, true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn starting_templates_skip_group_defaults() {
        let entries = vec![
            entry(template("plain", ""), false, "1"),
            entry(template("gone", ""), true, "2"),
            entry(template(&section_template_id("g", true), ""), false, "3"),
            entry(template(&section_template_id("g", false), ""), false, "4"),
            entry(me_groups(""), false, "5"),
        ];
        let ids: Vec<&str> = workspace::starting_templates(&entries)
            .into_iter()
            .map(KitEntry::id)
            .collect();
        assert_eq!(ids, ["plain"]);
    }

    #[test]
    fn local_drafts_are_kept_per_target() {
        let session = SettingsTarget::Session {
            id: "s".into(),
            name: "S".into(),
        };
        let section = SettingsTarget::Section {
            id: "g".into(),
            name: "G".into(),
        };
        let draft = SettingsTarget::Draft {
            key: "me:section:g".into(),
            section: Some("g".into()),
        };
        assert_eq!(session.draft_key(), "sessions:workspace-draft:session:s");
        assert_eq!(section.draft_key(), "me:workspace-draft:section:g");
        assert_eq!(draft.draft_key(), "me:section:g:settings");
        assert_eq!(
            local_draft(Some(json!({"canvas": "c", "base": null}))),
            Some(("c".into(), None, None))
        );
        assert_eq!(
            local_draft(Some(json!({"canvas": "c", "base": "r", "lineup": {}}))),
            Some(("c".into(), Some("r".into()), Some(json!({}))))
        );
        assert_eq!(local_draft(Some(json!({"canvas": "c"}))), None);
        assert_eq!(local_draft(Some(json!({"canvas": 1, "base": null}))), None);
        assert_eq!(local_draft(Some(json!({"canvas": "c", "base": 2}))), None);
    }

    /// Saves a session's Canvas and a Me group's defaults on the test
    /// relay and reads them back. Skipped unless `BUZZ_TEST_RELAY`
    /// points at `.test-relay`; signs in as a throwaway member.
    #[tokio::test(flavor = "multi_thread")]
    async fn saves_session_canvas_and_group_defaults_on_the_relay() {
        use buzz_relay_client::session::Expected;
        use buzz_relay_client::sidebar::NewSection;
        use buzz_relay_client::{RelayHttp, RelayOrigin, RosterStatus, Store, community};
        use std::time::Duration;
        let Ok(dir) = std::env::var("BUZZ_TEST_RELAY") else {
            eprintln!("BUZZ_TEST_RELAY not set; skipping");
            return;
        };
        let identities: Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{dir}/identities.json")).unwrap(),
        )
        .unwrap();
        let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
        let alice = Arc::new(
            buzz_identity::Identity::from_nsec(
                identities["identities"]["alice"]["nsec"].as_str().unwrap(),
            )
            .unwrap(),
        );
        let minted = RelayHttp::new(origin.clone(), alice)
            .post(
                "/api/invites",
                json!({"ttl_secs": 600, "max_uses": 1}).to_string(),
            )
            .await
            .unwrap();
        let minted: Value = serde_json::from_slice(&minted).unwrap();
        let viewer = Arc::new(buzz_identity::Identity::ephemeral());
        let http = RelayHttp::new(origin.clone(), viewer.clone());
        community::claim(&http, minted["code"].as_str().unwrap(), None, false)
            .await
            .unwrap();
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            origin,
            viewer,
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        tokio::time::timeout(
            Duration::from_secs(20),
            snapshot.wait_for(|s| s.roster == RosterStatus::Ready && s.live),
        )
        .await
        .unwrap()
        .unwrap();
        let channel = uuid::Uuid::new_v4().to_string();
        let op = session
            .work_session_create(&channel, "Settings", None)
            .await
            .unwrap();
        session.work_session_delivered(op).await.unwrap();
        session
            .work_session_refresh(&channel, Expected::default(), true)
            .await
            .unwrap();
        // Replaceable records keep second precision.
        let second = || tokio::time::sleep(Duration::from_millis(1100));

        // A session's Canvas, saved over the revision it was read at.
        let target = SettingsTarget::Session {
            id: channel.clone(),
            name: "Settings".into(),
        };
        let saved = read_saved(&session, &target).await.unwrap();
        assert_eq!(
            (saved.content.as_str(), saved.revision.as_ref()),
            ("", None)
        );
        let workspace = Workspace {
            folders: vec!["~/code/buzz".into()],
            worktree: Some(Worktree::default()),
            canvas: "Keep this context".into(),
        };
        let canvas = workspace_canvas(&workspace).unwrap();
        let lineup = Lineup::default();
        write_saved(&session, &target, &canvas, None, "", &lineup)
            .await
            .unwrap();
        let saved = read_saved(&session, &target).await.unwrap();
        assert_eq!(saved.content, canvas);
        assert_eq!(read_workspace(&saved.content).unwrap(), workspace);
        second().await;
        // A stale revision is refused and changes nothing.
        let error = write_saved(&session, &target, "Other", None, "", &lineup)
            .await
            .unwrap_err();
        assert!(
            error.starts_with("Canvas changed since you opened it."),
            "{error}"
        );
        write_saved(
            &session,
            &target,
            "Next",
            saved.revision.clone(),
            "",
            &lineup,
        )
        .await
        .unwrap();
        assert_eq!(read_saved(&session, &target).await.unwrap().content, "Next");

        // A Me group's defaults, as its own template record.
        let kit = session.me_kit();
        kit.admit(&channel, None).await.unwrap();
        kit.set_placement(&channel, true, None).await.unwrap();
        kit.me_ensure();
        let mut me = kit.watch_me();
        tokio::time::timeout(
            Duration::from_secs(20),
            me.wait_for(|s| s.status == PreferencesStatus::Ready),
        )
        .await
        .unwrap()
        .unwrap();
        let group = uuid::Uuid::new_v4().to_string();
        second().await;
        kit.me_create_and_assign(
            &channel,
            NewSection {
                id: group.clone(),
                name: "Launch".into(),
            },
        )
        .await
        .unwrap();
        let target = SettingsTarget::Section {
            id: group.clone(),
            name: "Launch".into(),
        };
        let saved = read_saved(&session, &target).await.unwrap();
        assert_eq!(saved.recipe_id, section_template_id(&group, true));
        assert_eq!(
            (saved.content.as_str(), saved.revision.as_ref()),
            ("", None)
        );
        let defaults = workspace_canvas(&Workspace {
            folders: vec!["~/code/buzz".into()],
            canvas: "Work carefully".into(),
            ..Workspace::default()
        })
        .unwrap();
        write_saved(
            &session,
            &target,
            &defaults,
            None,
            &saved.recipe_id,
            &saved.lineup,
        )
        .await
        .unwrap();
        let saved = read_saved(&session, &target).await.unwrap();
        assert_eq!(saved.content, defaults);
        let record = kit::template(&saved.entries, &saved.recipe_id).unwrap();
        assert_eq!(record.record["value"]["name"], "Launch chats");
        assert_eq!(kit::tag(&record.record), kit::ME_KIT_TAG);
        // New conversations in the group start from them.
        let setup = load_session_setup(&session, &group, true).await.unwrap();
        assert_eq!(setup.canvas, defaults);
        // A deleted record keeps its revision: defaults are recreated
        // over the tombstone.
        second().await;
        let value = record.record["value"].clone();
        kit.save(value, saved.revision.clone(), true).await.unwrap();
        let tombstone = read_saved(&session, &target).await.unwrap();
        assert_eq!(tombstone.content, "");
        assert!(tombstone.revision.is_some() && tombstone.revision != saved.revision);
        second().await;
        write_saved(
            &session,
            &target,
            "New instructions",
            tombstone.revision,
            &tombstone.recipe_id,
            &tombstone.lineup,
        )
        .await
        .unwrap();
        assert_eq!(
            read_saved(&session, &target).await.unwrap().content,
            "New instructions"
        );
    }
}
