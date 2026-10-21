//! What a plugin folder holds and what its program says (`docs/plugins.md`).
//!
//! A plugin is a program the app starts. It speaks JSON-RPC 2.0, one
//! message per line, on stdin and stdout, so a short script in any
//! language can be one. The app asks it what it contributes, opens views,
//! and forwards clicks and typing; the plugin answers with element trees
//! that the app draws with its own components.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The protocol version the app speaks.
pub const API_VERSION: u32 = 1;

/// The manifest's file name in a plugin folder.
pub const MANIFEST: &str = "buzz-plugin.json";

/// Longest line the app reads from a plugin.
pub const MAX_LINE: usize = 4 * 1024 * 1024;

const MAX_PANELS: usize = 16;
const MAX_PAGES: usize = 8;
const MAX_NODES: usize = 5_000;
const MAX_DEPTH: usize = 32;

/// `buzz-plugin.json`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Reverse-DNS style, such as `com.example.weather`. Unique per app.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The program and its arguments. A program with a `/` is relative to
    /// the plugin folder; otherwise it is found on `PATH`.
    pub command: Vec<String>,
    #[serde(default = "api_version")]
    pub api_version: u32,
    /// What the plugin may do as the user beyond reading. Settings shows
    /// these before the plugin is turned on, and the app refuses what is
    /// not declared.
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

fn api_version() -> u32 {
    API_VERSION
}

/// Something a plugin must declare before the app does it for the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// Send, edit, react to and delete messages as the user, and upload
    /// files to attach.
    SendMessages,
    /// Sign and publish other Nostr events as the user, of the kinds the
    /// app itself writes.
    PublishEvents,
    /// Create, change and remove the user's agents.
    ManageAgents,
    /// Start DMs and create, join, leave, archive and delete channels.
    ManageChannels,
}

impl Permission {
    /// How Settings describes it.
    pub fn describe(self) -> &'static str {
        match self {
            Permission::SendMessages => {
                "Send, edit, react to and delete messages, upload files and set reminders as you"
            }
            Permission::PublishEvents => {
                "Publish other events as you, such as your status, emoji and Git tokens"
            }
            Permission::ManageAgents => {
                "Create, change and remove your agents and teams, and their archive"
            }
            Permission::ManageChannels => {
                "Start DMs; create, join, leave, archive and delete channels; change members, \
                 details, workflows, your sidebar and Me"
            }
        }
    }
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_as(text, false)
    }

    /// A manifest Buzz ships with the app, which may use a `buzz.` id.
    pub fn parse_bundled(text: &str) -> Result<Self, String> {
        Self::parse_as(text, true)
    }

    fn parse_as(text: &str, bundled: bool) -> Result<Self, String> {
        let mut manifest: Self =
            serde_json::from_str(text).map_err(|error| format!("{MANIFEST}: {error}"))?;
        manifest
            .permissions
            .sort_by_key(|permission| *permission as u8);
        manifest.permissions.dedup();
        manifest.validate(bundled)?;
        Ok(manifest)
    }

    fn validate(&self, bundled: bool) -> Result<(), String> {
        if !valid_plugin_id(&self.id) {
            return Err(format!(
                "“{}” is not a plugin id. Use lowercase letters, digits, “.”, “-” and “_”, \
                 such as com.example.weather.",
                self.id
            ));
        }
        if !bundled && (self.id == "buzz" || self.id.starts_with("buzz.")) {
            return Err("Plugin ids starting with “buzz.” are reserved for Buzz.".into());
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 64 {
            return Err("The plugin name must be 1 to 64 characters.".into());
        }
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err("The manifest has no command to run.".into());
        }
        if self.command.len() > 64 || self.command.iter().any(|arg| arg.contains('\0')) {
            return Err("The manifest's command is not valid.".into());
        }
        if self.api_version != API_VERSION {
            return Err(format!(
                "This plugin needs plugin API {}; this app speaks {API_VERSION}.",
                self.api_version
            ));
        }
        Ok(())
    }

    /// The command as the import disclosure shows it.
    pub fn command_line(&self) -> String {
        self.command
            .iter()
            .map(|arg| {
                if arg.is_empty() || arg.contains(char::is_whitespace) {
                    format!("\"{arg}\"")
                } else {
                    arg.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn valid_plugin_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= 128
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// buzz-app's `[a-z0-9][a-z0-9._-]*`, up to 128 characters, as a
/// navigation target's ids may be.
pub fn valid_contribution_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= 128
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// The `initialize` result: what the plugin adds to the app.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contributions {
    #[serde(default)]
    pub panels: Vec<PanelDecl>,
    #[serde(default)]
    pub pages: Vec<PageDecl>,
    #[serde(default)]
    pub settings_cards: Vec<CardDecl>,
    #[serde(default)]
    pub notifications: Vec<CategoryDecl>,
    #[serde(default)]
    pub message_actions: Vec<ActionDecl>,
    #[serde(default)]
    pub link_renderers: Vec<LinkRendererDecl>,
    #[serde(default)]
    pub message_renderers: Vec<MessageRendererDecl>,
    #[serde(default)]
    pub attachment_renderers: Vec<AttachmentRendererDecl>,
    #[serde(default)]
    pub composer_tools: Vec<ToolDecl>,
    #[serde(default)]
    pub shortcuts: Vec<ShortcutDecl>,
    #[serde(default)]
    pub completions: Vec<CompletionDecl>,
    #[serde(default)]
    pub inline_renderers: Vec<InlineRendererDecl>,
    #[serde(default)]
    pub account_actions: Vec<AccountActionDecl>,
    #[serde(default)]
    pub naming_policies: Vec<NamingPolicyDecl>,
    /// What the plugin offers other plugins (`ctx.provide`): they call it
    /// with `services/request`, and Buzz asks this plugin
    /// `service/request`.
    #[serde(default)]
    pub services: Vec<ServiceDecl>,
    #[serde(default)]
    pub composer_accessories: Vec<AccessoryDecl>,
    #[serde(default)]
    pub agent_tools: Vec<AgentToolDecl>,
    #[serde(default)]
    pub agent_types: Vec<AgentTypeDecl>,
}

/// A naming policy (`identityNames.register`): Buzz asks `names/resolve`
/// for display names. It applies only while it is the one policy on.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct NamingPolicyDecl {
    pub id: String,
}

/// A service a plugin offers other plugins.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServiceDecl {
    pub id: String,
}

/// A row in the account menu that opens a dialog of kind `dialog`
/// (`accountActions.register`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AccountActionDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
}

/// A view above every composer, beside the typing indicator, of kind
/// `accessory` (`conversation.registerAccessory`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AccessoryDecl {
    pub id: String,
    pub title: String,
    #[serde(default, deserialize_with = "order")]
    pub order: i32,
    /// `composer` (the default): a view above every composer. `message`:
    /// drawn beside each message's reactions; Buzz asks
    /// `message/accessory`.
    #[serde(default)]
    pub placement: AccessoryPlacement,
    /// For `message`: the message kinds it shows on; chat messages when
    /// empty.
    #[serde(default)]
    pub kinds: Vec<u16>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccessoryPlacement {
    #[default]
    Composer,
    Message,
}

/// Draws pieces of message text that `pattern` (a regular expression)
/// matches, such as ticket numbers (`conversation.registerInline`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineRendererDecl {
    pub id: String,
    /// What Settings and the contribution readers call it.
    #[serde(default)]
    pub title: Option<String>,
    pub pattern: String,
}

impl InlineRendererDecl {
    /// The compiled pattern, bounded so a plugin cannot make prose slow.
    pub fn regex(&self) -> Result<regex::Regex, String> {
        if self.pattern.chars().count() > 500 {
            return Err(format!("“{}”'s pattern is over 500 characters.", self.id));
        }
        let regex = regex::RegexBuilder::new(&self.pattern)
            .size_limit(1 << 20)
            .build()
            .map_err(|error| format!("“{}”'s pattern is not valid: {error}", self.id))?;
        if regex.is_match("") {
            return Err(format!("“{}”'s pattern matches empty text.", self.id));
        }
        Ok(regex)
    }
}

/// A tool every agent session gets (`agents.addTool`). Calling it sends
/// `agentTool/call`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolDecl {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A kind of agent the plugin runs (`agents2.register`). Each delivery
/// sends `agent/run`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTypeDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    /// A new agent's settings.
    #[serde(default)]
    pub config: Option<Value>,
    /// A new agent's standing instructions, as its default Interest.
    #[serde(default)]
    pub instructions: Option<String>,
    /// More attention objects a new agent starts with, by slug, as
    /// `agents/save` takes them (watches, timers, interests).
    #[serde(default)]
    pub attention: serde_json::Map<String, Value>,
    /// How long one run may take, at most 30 minutes; a minute by default,
    /// as buzz-app's `timeoutMs`.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Whether Buzz asks `agent/summary` for the line under an agent's
    /// name; without it the line is the title.
    #[serde(default)]
    pub summary: bool,
    /// Whether Buzz asks `agent/peek` for a glance at a selected agent.
    #[serde(default)]
    pub peek: bool,
    /// Settings tabs before Attention, each a view of kind `agentTab`.
    #[serde(default)]
    pub tabs: Vec<AgentTabDecl>,
    /// A Buzz icon shown with the type, such as `Sparkle`; the robot
    /// without one.
    #[serde(default)]
    pub icon: Option<String>,
    /// The New agent dialog offers the new agent's instructions.
    #[serde(default)]
    pub create_instructions: bool,
    /// The type's own part of the New agent dialog: a view of kind
    /// `agentCreate` that sets the new agent's config with
    /// `agent/createConfig`.
    #[serde(default)]
    pub create_form: bool,
    /// Model choices in the New agent dialog, saved as the config's
    /// `model`.
    #[serde(default)]
    pub create_models: Option<CreateModelsDecl>,
    /// Whether Buzz asks `agent/models` for the models and efforts the
    /// owner can pick while mentioning an agent.
    #[serde(default)]
    pub models: bool,
    /// Device-wide readiness under Provider settings: the plugin reports
    /// it with `agentType/readiness`, and its dialog is a view of kind
    /// `agentProvider`.
    #[serde(default)]
    pub provider: bool,
    /// The config keys that hold what another type may take over when an
    /// agent changes type.
    #[serde(default)]
    pub carry: Option<CarryDecl>,
    /// What leaving this type costs, shown before a change of type.
    #[serde(default)]
    pub migration_warning: Option<String>,
    /// The settings a change of type would reset or drop.
    #[serde(default)]
    pub migration_settings: Vec<MigrationSettingDecl>,
    /// The plugin holds each agent's key and runs it (`remote`): Buzz asks
    /// `agent/provision`, `agent/activate`, `agent/profile` and
    /// `agent/remove`, and never `agent/run`.
    #[serde(default)]
    pub hosted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateModelsDecl {
    #[serde(default)]
    pub models: Vec<ModelChoiceDecl>,
    /// The default row's label.
    #[serde(default)]
    pub default_label: Option<String>,
    /// Whether an id not listed may be entered.
    #[serde(default)]
    pub allow_custom: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ModelChoiceDecl {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct CarryDecl {
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AgentTabDecl {
    pub id: String,
    pub title: String,
    /// `settings`: the tab is the agent's Settings, with Buzz's Provider
    /// field at its top (`AgentTab.role`).
    #[serde(default)]
    pub role: Option<AgentTabRole>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentTabRole {
    Settings,
}

/// A setting a provider-change preview names (`describeMigration`): it
/// is the agent's own when its config has `key` other than `default`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationSettingDecl {
    pub id: String,
    pub label: String,
    pub key: String,
    #[serde(default)]
    pub default: Option<Value>,
}

fn one() -> u32 {
    1
}

/// Suggestions after a trigger character typed in the composer
/// (`conversation.registerCompletion`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionDecl {
    pub id: String,
    pub title: String,
    /// One character; `@`, `#` and `:` only by ranking before Buzz's own.
    pub trigger: String,
    /// Of completions on one trigger, the lowest order (then key) answers.
    #[serde(default, deserialize_with = "order")]
    pub order: i32,
}

/// A panel beside the conversation (`panels.register`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
    /// Link prefixes the panel shows, such as `https://example.com/`. A
    /// matching link clicked in a message opens the panel.
    #[serde(default)]
    pub matches: Vec<String>,
    /// A regular expression a link must also match, for panels that show
    /// only some links under a prefix.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Which panel shows a link several match: the lowest wins.
    #[serde(default, deserialize_with = "order")]
    pub order: i32,
    /// A button in the channel header that toggles the panel.
    #[serde(default)]
    pub launcher: Option<LauncherDecl>,
    /// A button in the app's header that opens the panel at `target`
    /// beside any page (`Panel.launcher`).
    #[serde(default)]
    pub app_launcher: Option<AppLauncherDecl>,
    /// An entry in the channel's menu that opens the panel as a tab.
    #[serde(default)]
    pub menu: Option<MenuDecl>,
    /// `workspace` draws the panel edge to edge, as one with its own
    /// header; `document` (the default) pads it.
    #[serde(default)]
    pub layout: PageLayout,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherDecl {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub placement: LauncherPlacement,
    /// What the panel opens showing; the channel's id when absent.
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLauncherDecl {
    #[serde(default)]
    pub label: Option<String>,
    pub target: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LauncherPlacement {
    Side,
    /// The bottom drawer, as buzz-app's `channelPlacement` defaults to.
    #[default]
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuDecl {
    pub label: String,
    /// The kinds of channel it is offered in (`stream`, `forum`, `dm`,
    /// `session`); every kind when empty.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Only private (`true`) or public (`false`) channels.
    #[serde(default)]
    pub private: Option<bool>,
    /// Only channels you are a member of.
    #[serde(default)]
    pub member: bool,
    /// Only live channels you can write in: not this device's saved
    /// copy, read-only or archived.
    #[serde(default)]
    pub writable: bool,
    /// Only while this device saves your agents' activity
    /// (`agentActivity.archive`).
    #[serde(default)]
    pub archive: bool,
}

/// A page (`pages.register`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub placement: PagePlacement,
    /// Takes routes: `ui/navigate` can open it at `params`, and the page's
    /// view hears `page/route`.
    #[serde(default)]
    pub routes: bool,
    /// What its route params mean: raise it when they change, so targets
    /// made for the old ones are refused.
    #[serde(default = "one")]
    pub route_version: u32,
    /// `document` (the default) pads the page; `workspace` gives the plugin
    /// the whole area, edge to edge.
    #[serde(default)]
    pub layout: PageLayout,
    /// The visit stays opening until the page sends `page/settle`
    /// (`handlesNavigation`).
    #[serde(default)]
    pub handles_navigation: bool,
    /// Shows Buzz's companion dock beside the page (`companion`).
    #[serde(default)]
    pub companion: bool,
    /// `false` keeps the page out of navigation (buzz-app's `primary`).
    #[serde(default)]
    pub primary: Option<bool>,
}

/// An `order`: any finite number, as buzz-app takes, rounded to a whole
/// one.
fn order<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i32, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if !value.is_finite() {
        return Err(serde::de::Error::custom("order must be a finite number"));
    }
    Ok(value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PagePlacement {
    /// A row at the top of the channel sidebar.
    #[default]
    Sidebar,
    /// A pill in the middle of the header.
    Topbar,
    /// buzz-app's header toolbar icon; the header pill here.
    Toolbar,
    /// No navigation entry; search and links still reach it.
    Hidden,
    /// A placement Buzz does not know: dropped, as buzz-app drops it,
    /// leaving the default.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PageLayout {
    #[default]
    Document,
    Workspace,
}

/// A Settings section (`settingsCards.register`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
    /// `community` (under the selected community, the default, as a card
    /// with no `group` or `section`), `administration` (its
    /// Administration group) or the name of an account-level group, such
    /// as `Integrations`.
    #[serde(default = "community")]
    pub placement: String,
}

fn community() -> String {
    "community".into()
}

/// A notification category with its own switch (`notifications.register`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryDecl {
    pub id: String,
    pub label: String,
}

/// An item in a message's More menu, and optionally a marker on the
/// messages the plugin marks (`conversation.registerMessageAction`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
    /// Event kinds it applies to; empty for chat messages (kind 9).
    #[serde(default)]
    pub kinds: Vec<u16>,
    /// Only the user's own messages.
    #[serde(default)]
    pub mine: bool,
    /// Only messages with (`true`) or without (`false`) attachments.
    #[serde(default)]
    pub attachments: Option<bool>,
    /// Only thread replies (`true`) or top-level messages (`false`).
    #[serde(default)]
    pub replies: Option<bool>,
    /// Only messages whose text this regular expression finds.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Only messages in these channels.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Only messages by these people (pubkeys), such as your agents.
    #[serde(default)]
    pub authors: Vec<String>,
    /// Only messages with (`true`) or without (`false`) reactions.
    #[serde(default)]
    pub reactions: Option<bool>,
}

/// Draws link chips for URLs with these prefixes.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRendererDecl {
    pub id: String,
    /// What Settings and the contribution readers call it.
    #[serde(default)]
    pub title: Option<String>,
    pub matches: Vec<String>,
    /// Lower wins among renderers that match.
    #[serde(default, deserialize_with = "order")]
    pub order: i32,
}

/// Replaces the body of messages of these kinds whose content starts
/// with `prefix`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRendererDecl {
    pub id: String,
    /// What Settings and the contribution readers call it.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kinds: Vec<u16>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default, deserialize_with = "order")]
    pub order: i32,
}

/// Replaces the player of attachments with these MIME type prefixes.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRendererDecl {
    pub id: String,
    /// What Settings and the contribution readers call it.
    #[serde(default)]
    pub title: Option<String>,
    pub mime: Vec<String>,
}

/// A button in every composer's toolbar (`conversation.registerTool`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "order")]
    pub order: i32,
    /// Also a reaction picker (`reactionComponent`): a view of kind
    /// `reactionPicker` shown for "Add reaction" when its order (then
    /// key) comes before Buzz's emoji picker at 0.
    #[serde(default)]
    pub reaction_picker: bool,
}

/// A keyboard shortcut (`shortcuts.register`), listed and rebindable in
/// Settings → Shortcuts.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutDecl {
    pub id: String,
    pub title: String,
    /// Chords such as `mod+shift+k`; `mod` is Command on macOS and Control
    /// elsewhere.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Settings lists the plugin's shortcuts by this, then by id.
    #[serde(default, deserialize_with = "order")]
    pub order: i32,
    /// Also runs while a text field has focus.
    #[serde(default)]
    pub allow_in_editable: bool,
    /// Also runs while a dialog is open.
    #[serde(default)]
    pub allow_in_modal: bool,
    /// Runs again while the chord is held.
    #[serde(default)]
    pub repeat: bool,
}

/// A chord as `mod+shift+alt+key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub key: String,
    pub primary: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Chord {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut chord = Chord {
            key: String::new(),
            primary: false,
            shift: false,
            alt: false,
        };
        // `mod++` and a bare `+` bind the plus key itself.
        let (head, plus) = match text.strip_suffix("++") {
            Some(head) => (head, true),
            None if text.trim() == "+" => ("", true),
            None => (text, false),
        };
        let mut parts: Vec<&str> = if head.is_empty() {
            Vec::new()
        } else {
            head.split('+').map(str::trim).collect()
        };
        if plus {
            parts.push("+");
        }
        let (key, modifiers) = parts.split_last().ok_or("an empty chord")?;
        for modifier in modifiers {
            match modifier.to_ascii_lowercase().as_str() {
                "mod" | "cmd" | "ctrl" => chord.primary = true,
                "shift" => chord.shift = true,
                "alt" | "option" => chord.alt = true,
                other => return Err(format!("“{other}” is not a modifier in “{text}”.")),
            }
        }
        if key.is_empty() || key.chars().count() > 16 {
            return Err(format!("“{text}” has no key."));
        }
        // Keys typed with Shift, such as `?`, arrive without it, as the
        // symbol alone; a browser's `shift+?` is the same press.
        if key.chars().count() == 1 && key.to_lowercase() == key.to_uppercase() {
            chord.shift = false;
        }
        chord.key = key.to_string();
        Ok(chord)
    }
}

/// How many of each a plugin can add.
const MAX_EACH: usize = 16;

fn check_title(id: &str, title: &str) -> Result<(), String> {
    if title.trim().is_empty() || title.chars().count() > 64 {
        return Err(format!("“{id}” needs a title of 1 to 64 characters."));
    }
    Ok(())
}

fn check_prefixes(id: &str, prefixes: &[String]) -> Result<(), String> {
    if prefixes.is_empty() || prefixes.iter().any(|prefix| prefix.len() < 4) {
        return Err(format!(
            "“{id}” matches links too broadly. Use a prefix such as https://example.com/."
        ));
    }
    Ok(())
}

impl Contributions {
    pub fn parse(value: Value) -> Result<Self, String> {
        let contributions: Self = serde_json::from_value(value)
            .map_err(|error| format!("The plugin's initialize result is not valid: {error}"))?;
        contributions.validate()?;
        Ok(contributions)
    }

    fn validate(&self) -> Result<(), String> {
        if self.panels.len() > MAX_PANELS || self.pages.len() > MAX_PAGES {
            return Err(format!(
                "A plugin can add at most {MAX_PANELS} panels and {MAX_PAGES} pages."
            ));
        }
        let counts = [
            self.settings_cards.len(),
            self.notifications.len(),
            self.message_actions.len(),
            self.link_renderers.len(),
            self.message_renderers.len(),
            self.attachment_renderers.len(),
            self.composer_tools.len(),
            self.shortcuts.len(),
            self.completions.len(),
            self.agent_tools.len(),
            self.inline_renderers.len(),
            self.account_actions.len(),
            self.composer_accessories.len(),
            self.agent_types.len(),
        ];
        if counts.iter().any(|count| *count > MAX_EACH) {
            return Err(format!("A plugin can add at most {MAX_EACH} of each kind."));
        }
        let mut seen = std::collections::HashSet::new();
        let mut id = |id: &str| -> Result<(), String> {
            if !valid_contribution_id(id) {
                return Err(format!(
                    "“{id}” is not a contribution id. Use lowercase letters, digits, “-” and “_”."
                ));
            }
            if !seen.insert(id.to_owned()) {
                return Err(format!("The plugin adds “{id}” twice."));
            }
            Ok(())
        };
        for panel in &self.panels {
            id(&panel.id)?;
            check_title(&panel.id, &panel.title)?;
            if !panel.matches.is_empty() {
                check_prefixes(&panel.id, &panel.matches)?;
            }
            if let Some(pattern) = &panel.pattern
                && (pattern.chars().count() > 500 || regex::Regex::new(pattern).is_err())
            {
                return Err(format!(
                    "“{}” needs a pattern of at most 500 characters that compiles.",
                    panel.id
                ));
            }
            if let Some(menu) = &panel.menu {
                check_title(&panel.id, &menu.label)?;
            }
        }
        for page in &self.pages {
            id(&page.id)?;
            check_title(&page.id, &page.title)?;
            if page.route_version == 0 {
                return Err(format!("“{}”'s route version must be 1 or more.", page.id));
            }
        }
        for card in &self.settings_cards {
            id(&card.id)?;
            check_title(&card.id, &card.title)?;
            check_title(&card.id, &card.placement)?;
        }
        // A notification category is keyed apart from what draws, so it
        // may share an id with one, as a page and its category do.
        let mut categories = std::collections::HashSet::new();
        for category in &self.notifications {
            if !valid_contribution_id(&category.id) {
                return Err(format!(
                    "“{}” is not a contribution id. Use lowercase letters, digits, “-” and “_”.",
                    category.id
                ));
            }
            if !categories.insert(category.id.as_str()) {
                return Err(format!("The plugin adds “{}” twice.", category.id));
            }
            // Labels may run to 128 characters, as buzz-app's.
            if category.label.trim().is_empty() || category.label.encode_utf16().count() > 128 {
                return Err(format!(
                    "“{}” needs a label of 1 to 128 characters.",
                    category.id
                ));
            }
        }
        for action in &self.message_actions {
            id(&action.id)?;
            check_title(&action.id, &action.title)?;
            if let Some(pattern) = &action.pattern
                && (pattern.chars().count() > 500 || regex::Regex::new(pattern).is_err())
            {
                return Err(format!(
                    "“{}” needs a pattern of at most 500 characters that compiles.",
                    action.id
                ));
            }
            if action.authors.len() > 500
                || action.authors.iter().any(|author| {
                    author.len() != 64 || !author.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return Err(format!(
                    "“{}”'s authors must be at most 500 pubkeys.",
                    action.id
                ));
            }
        }
        for renderer in &self.link_renderers {
            id(&renderer.id)?;
            check_prefixes(&renderer.id, &renderer.matches)?;
        }
        for renderer in &self.message_renderers {
            id(&renderer.id)?;
        }
        for renderer in &self.attachment_renderers {
            id(&renderer.id)?;
            if renderer.mime.is_empty() || renderer.mime.iter().any(|mime| mime.len() < 3) {
                return Err(format!(
                    "“{}” needs MIME type prefixes, such as audio/.",
                    renderer.id
                ));
            }
        }
        for tool in &self.composer_tools {
            id(&tool.id)?;
            check_title(&tool.id, &tool.title)?;
        }
        for completion in &self.completions {
            id(&completion.id)?;
            check_title(&completion.id, &completion.title)?;
            let mut chars = completion.trigger.chars();
            let one = match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            };
            if one.is_none_or(|c| c.is_alphanumeric() || c.is_whitespace()) {
                return Err(format!(
                    "“{}” needs one trigger character that is not a letter, digit or space.",
                    completion.id
                ));
            }
        }
        for renderer in &self.inline_renderers {
            id(&renderer.id)?;
            renderer.regex()?;
        }
        if self.naming_policies.len() > 1 {
            return Err("A plugin can add one naming policy.".to_owned());
        }
        for policy in &self.naming_policies {
            id(&policy.id)?;
        }
        for service in &self.services {
            id(&service.id)?;
        }
        for action in &self.account_actions {
            id(&action.id)?;
            check_title(&action.id, &action.title)?;
        }
        for accessory in &self.composer_accessories {
            id(&accessory.id)?;
            check_title(&accessory.id, &accessory.title)?;
        }
        let mut names = std::collections::HashSet::new();
        for tool in &self.agent_tools {
            if !crate::buzz_mcp::added::valid_name(&tool.name) {
                return Err(format!(
                    "“{}” is not a tool name. Use a lowercase letter, then lowercase letters, \
                     digits or _.",
                    tool.name
                ));
            }
            if !names.insert(&tool.name) {
                return Err(format!(
                    "The plugin adds the agent tool “{}” twice.",
                    tool.name
                ));
            }
            if tool.description.trim().is_empty() || tool.description.chars().count() > 2_000 {
                return Err(format!(
                    "“{}” needs a description of up to 2000 characters.",
                    tool.name
                ));
            }
            if tool.input_schema.get("type").and_then(Value::as_str) != Some("object")
                || tool.input_schema.to_string().len() > 32 * 1024
            {
                return Err(format!(
                    "“{}” needs an object inputSchema under 32 KiB.",
                    tool.name
                ));
            }
        }
        for kind in &self.agent_types {
            id(&kind.id)?;
            check_title(&kind.id, &kind.title)?;
            if kind
                .config
                .as_ref()
                .is_some_and(|config| !config.is_object())
            {
                return Err(format!("“{}”'s config must be an object.", kind.id));
            }
            // A hosted type's provider runs it: no models to pick, no
            // run time of Buzz's to bound (`remote`).
            if kind.hosted && (kind.models || kind.timeout_seconds.is_some()) {
                return Err(format!(
                    "“{}” is hosted, so it takes no models or timeout.",
                    kind.id
                ));
            }
            if kind
                .timeout_seconds
                .is_some_and(|timeout| !(1..=1_800).contains(&timeout))
            {
                return Err(format!(
                    "“{}”'s timeout must be 1 to 1800 seconds.",
                    kind.id
                ));
            }
            if kind.tabs.len() > 8 {
                return Err(format!("“{}” has more than 8 tabs.", kind.id));
            }
            let settings = kind.tabs.iter().filter(|tab| tab.role.is_some());
            if settings.count() > 1 {
                return Err(format!("“{}” has more than one Settings tab.", kind.id));
            }
            if kind.migration_settings.len() > 32 {
                return Err(format!(
                    "“{}” has more than 32 migration settings.",
                    kind.id
                ));
            }
            let mut tabs = std::collections::HashSet::new();
            for tab in &kind.tabs {
                // The app's own tabs keep their ids.
                if !valid_contribution_id(&tab.id)
                    || matches!(tab.id.as_str(), "attention" | "memory" | "settings")
                {
                    return Err(format!("“{}” is not a tab id.", tab.id));
                }
                if !tabs.insert(&tab.id) {
                    return Err(format!("“{}” has the tab “{}” twice.", kind.id, tab.id));
                }
                check_title(&tab.id, &tab.title)?;
            }
        }
        for shortcut in &self.shortcuts {
            id(&shortcut.id)?;
            check_title(&shortcut.id, &shortcut.title)?;
            if shortcut.keys.is_empty() {
                return Err(format!("“{}” needs at least one key binding.", shortcut.id));
            }
            for keys in &shortcut.keys {
                Chord::parse(keys)?;
            }
        }
        Ok(())
    }
}

/// One element of a view, as the plugin sends it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Node {
    Column {
        #[serde(default)]
        children: Vec<Node>,
        #[serde(default)]
        gap: Gap,
        /// Where children sit across the column (default `stretch`).
        #[serde(default)]
        align: Align,
        /// Where children sit down the column.
        #[serde(default)]
        justify: Justify,
        /// Takes the height left in its parent column, such as to centre
        /// something on a page.
        #[serde(default)]
        grow: bool,
        /// Space inside its edges.
        #[serde(default = "no_gap")]
        padding: Gap,
        #[serde(default)]
        background: Option<Background>,
    },
    Row {
        #[serde(default)]
        children: Vec<Node>,
        #[serde(default)]
        gap: Gap,
        #[serde(default)]
        justify: Justify,
        #[serde(default)]
        wrap: bool,
        /// Takes the width left in its parent row.
        #[serde(default)]
        grow: bool,
        #[serde(default = "no_gap")]
        padding: Gap,
        #[serde(default)]
        background: Option<Background>,
    },
    /// A page or panel's title bar, as Buzz's own pages have, with
    /// `children` as its actions.
    Header {
        title: String,
        #[serde(default)]
        icon: Option<String>,
        /// A caption after the title, such as the channel's name.
        #[serde(default)]
        subtitle: Option<String>,
        #[serde(default)]
        children: Vec<Node>,
    },
    Card {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        children: Vec<Node>,
        /// Space inside its border (default `md`).
        #[serde(default = "md_gap")]
        padding: Gap,
        #[serde(default)]
        gap: Gap,
    },
    Text {
        text: String,
        #[serde(default)]
        style: TextStyle,
        #[serde(default)]
        tone: Tone,
        /// One line, cut with an ellipsis.
        #[serde(default)]
        truncate: bool,
        /// Shown on hover, such as a time's full date.
        #[serde(default)]
        tooltip: Option<String>,
    },
    Markdown {
        text: String,
    },
    Button {
        id: String,
        label: String,
        #[serde(default)]
        variant: ButtonStyle,
        #[serde(default)]
        icon: Option<String>,
        #[serde(default)]
        disabled: bool,
        #[serde(default)]
        loading: bool,
        /// `sm` for a small button; without a label, an icon button.
        #[serde(default)]
        size: Option<String>,
        /// Shown on hover, such as an icon button's name.
        #[serde(default)]
        tooltip: Option<String>,
        /// Takes the full width of its column.
        #[serde(default)]
        fill: bool,
    },
    Link {
        label: String,
        url: String,
        /// Opens in the browser even when a panel shows the link.
        #[serde(default)]
        external: bool,
    },
    /// A text field. The app keeps what is typed, so typing never waits
    /// on the plugin; each change and Enter is sent as an event.
    Input {
        id: String,
        /// The text the field starts with.
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
        /// Bumping it puts `value` into the field again.
        #[serde(default)]
        version: u64,
        /// Empties the field when Enter sends it, without waiting on the
        /// plugin.
        #[serde(default)]
        clear_on_submit: bool,
        #[serde(default)]
        disabled: bool,
        /// Takes the width left in its row.
        #[serde(default)]
        grow: bool,
    },
    /// A text area of several lines; each change is sent as an event.
    Textarea {
        id: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
        /// Bumping it puts `value` into the field again.
        #[serde(default)]
        version: u64,
        /// Lines it shows at least (default 3) and grows to (default 8).
        #[serde(default)]
        rows: Option<u32>,
        #[serde(default)]
        max_rows: Option<u32>,
        #[serde(default)]
        disabled: bool,
    },
    Switch {
        id: String,
        checked: bool,
        #[serde(default)]
        label: Option<String>,
    },
    /// Sends `toggle` with `true` or `false`.
    Checkbox {
        id: String,
        checked: bool,
        #[serde(default)]
        label: Option<String>,
        /// Drawn part-way, as a task in progress.
        #[serde(default)]
        indeterminate: bool,
        /// The label struck through and muted, as a finished task.
        #[serde(default)]
        completed: bool,
        #[serde(default)]
        disabled: bool,
    },
    /// One of `options`, as radio buttons; sends `change` with the chosen
    /// option's `value`.
    Choice {
        id: String,
        #[serde(default)]
        value: Option<String>,
        options: Vec<ChoiceOption>,
    },
    /// A bar filled to `value`, from 0 to 1.
    Progress {
        value: f32,
    },
    /// An image: community media or an https URL, scaled to fit `width`
    /// and `height` (pixels, up to 1200).
    Image {
        url: String,
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
        #[serde(default)]
        alt: Option<String>,
    },
    /// Buzz's audio player, or its voice note card with `voice`, for the
    /// community's media or a draft's local URL (`AttachmentRendererProps
    /// .source`).
    Audio {
        url: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        mime: Option<String>,
        /// Seconds.
        #[serde(default)]
        duration: Option<f64>,
        /// 0 to 255 per bar.
        #[serde(default)]
        waveform: Option<Vec<u8>>,
        #[serde(default)]
        voice: bool,
    },
    /// Buzz's own composer for a channel, or a thread in it
    /// (`conversation.ui.Composer`). Sending works as in the channel and
    /// then sends `sent`; with `submit: "plugin"`, the draft goes to the
    /// plugin as `submit` instead.
    Composer {
        id: String,
        /// Empty for a draft not in a conversation yet, which the plugin
        /// takes (`submit: "plugin"`), such as a new conversation's first
        /// message (`ui.Composer` `submission`).
        #[serde(default)]
        channel: String,
        #[serde(default)]
        thread: Option<String>,
        /// The reply a thread composer answers; the root by default.
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        submit: Option<String>,
        #[serde(default)]
        disabled: bool,
        #[serde(default)]
        auto_focus: bool,
        /// The text it starts with, and puts back when it changes.
        #[serde(default)]
        draft: Option<String>,
        /// People it starts with as mention chips, `[{"pubkey","name"}]`.
        #[serde(default)]
        mentions: Vec<Value>,
        /// Offers your library agents not yet in the conversation.
        #[serde(default)]
        invite_agents: bool,
        /// Where a draft without a channel is kept, so it comes back.
        #[serde(default)]
        draft_key: Option<String>,
    },
    /// One message as Buzz draws it in a conversation
    /// (`conversation.ui.Message`), for display.
    Message {
        id: String,
        channel: String,
        message: String,
        /// `timeline` (the default), `thread` or `nested`: the row as a
        /// conversation, a thread panel or a nested reply draws it.
        #[serde(default)]
        layout: Option<String>,
        /// Without the avatar and byline, as a follow-up in a group.
        #[serde(default)]
        compact: bool,
        /// The hover bar and its More menu (default on).
        #[serde(default)]
        actions: Option<bool>,
    },
    /// Buzz's own thread view of `root` in `channel`
    /// (`conversation.ui.Thread`), `height` pixels tall (default 480).
    /// Closing it sends `close`.
    Thread {
        id: String,
        channel: String,
        root: String,
        #[serde(default)]
        height: Option<u32>,
    },
    /// Someone's profile picture, or their initial; or, with `url`, a
    /// picture from the web with `name`'s initial behind it.
    Avatar {
        #[serde(default)]
        pubkey: String,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        size: AvatarSize,
    },
    Badge {
        text: String,
        #[serde(default)]
        tone: Tone,
    },
    Icon {
        name: String,
        /// Pixels, 8 to 128 (default 16).
        #[serde(default)]
        size: Option<u32>,
        #[serde(default)]
        tone: Tone,
    },
    /// A small button that opens a menu of `options`; choosing one sends
    /// `change` with its `value`.
    Menu {
        id: String,
        /// A button with this label, in place of a small icon button.
        #[serde(default)]
        label: Option<String>,
        /// The button's icon, or `avatar`'s picture.
        #[serde(default)]
        icon: Option<String>,
        #[serde(default)]
        icon_tone: Tone,
        #[serde(default)]
        avatar: Option<String>,
        #[serde(default)]
        tooltip: Option<String>,
        /// The menu's heading.
        #[serde(default)]
        title: Option<String>,
        /// The option shown chosen.
        #[serde(default)]
        value: Option<String>,
        options: Vec<MenuOption>,
        #[serde(default)]
        disabled: bool,
    },
    /// A field that opens a list of `options`; choosing one sends
    /// `change` with its `value`.
    Select {
        id: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
        options: Vec<MenuOption>,
        #[serde(default)]
        disabled: bool,
        /// Pixels; it fills its place by default.
        #[serde(default)]
        width: Option<u32>,
    },
    /// A small button that shows a note under it.
    Popover {
        id: String,
        #[serde(default)]
        icon: Option<String>,
        #[serde(default)]
        tooltip: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
    },
    /// An empty list's placeholder: a bordered card with an icon, title
    /// and description (`EmptyState`).
    Empty {
        #[serde(default)]
        icon: Option<String>,
        title: String,
        #[serde(default)]
        description: Option<String>,
    },
    Divider,
    Spinner,
    /// Monospace lines, such as code or a diff, each a row of cells.
    Code {
        lines: Vec<CodeLine>,
        /// Long lines wrap (default); otherwise the block scrolls sideways.
        #[serde(default = "yes")]
        wrap: bool,
        /// Pixels, up to 2400: narrower, it scrolls sideways.
        #[serde(default)]
        min_width: Option<u32>,
    },
    /// Its children, scrolling once taller than `max_height` pixels.
    Scroll {
        #[serde(default)]
        children: Vec<Node>,
        max_height: u32,
        #[serde(default = "no_gap")]
        padding: Gap,
    },
    /// GitHub-flavoured Markdown, as GitHub shows an issue, pull request or
    /// comment body: `html` is the HTML GitHub rendered for it, which tells
    /// its media apart, and links resolve against `base`.
    Document {
        text: String,
        #[serde(default)]
        html: Option<String>,
        #[serde(default)]
        base: Option<String>,
    },
    /// A message that opens to show its body: a `marker` in the gutter, a
    /// `header`, a one-line `preview` while closed and `children` while
    /// open. With `fit`, it does not open when that text fits the header on
    /// one line, and shows `children` in place of the preview.
    Collapsible {
        id: String,
        #[serde(default)]
        open: bool,
        #[serde(default)]
        marker: Vec<Node>,
        #[serde(default)]
        marker_tooltip: Option<String>,
        #[serde(default)]
        header: Vec<Node>,
        #[serde(default)]
        preview: Option<String>,
        #[serde(default)]
        fit: Option<String>,
        #[serde(default = "yes")]
        expandable: bool,
        #[serde(default)]
        children: Vec<Node>,
        /// Under the header while closed, such as image thumbnails.
        #[serde(default)]
        below: Vec<Node>,
        /// At the header's end, shown on hover.
        #[serde(default)]
        actions: Vec<Node>,
        /// Under the body while open.
        #[serde(default)]
        footer: Vec<Node>,
    },
    /// Square image thumbnails, as many as fit the width, the last
    /// counting the rest; each opens the media viewer.
    Thumbnails {
        images: Vec<String>,
        #[serde(default)]
        label: Option<String>,
    },
    /// Tabs over a panel's content; choosing one sends `change` with its
    /// `value`.
    Tabs {
        id: String,
        #[serde(default)]
        value: Option<String>,
        options: Vec<ChoiceOption>,
    },
    /// A ring of coloured arcs, one per segment, from the top: a summary
    /// such as checks by outcome.
    Ring {
        segments: Vec<RingSegment>,
        /// Pixels, 16 to 128 (default 32).
        #[serde(default)]
        size: Option<u32>,
    },
    /// An element this app does not know; it draws nothing.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Gap {
    None,
    #[default]
    Sm,
    Md,
    Lg,
}

fn no_gap() -> Gap {
    Gap::None
}

fn md_gap() -> Gap {
    Gap::Md
}

fn yes() -> bool {
    true
}

/// A fill behind a row or column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Background {
    /// Set into the surface, as a card's header.
    Inset,
    Panel,
}

/// One line of a `code` block: `text` and `tone`, or `cells`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CodeLine {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tone: CodeTone,
    #[serde(default)]
    pub cells: Vec<CodeCell>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CodeCell {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tone: CodeTone,
    #[serde(default)]
    pub kind: CellKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodeTone {
    Added,
    Removed,
    Hunk,
    Meta,
    #[default]
    #[serde(other)]
    Context,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CellKind {
    /// A line number, right-aligned in a 7-character gutter.
    Number,
    /// A gutter with no line on this side, as a split diff draws one.
    Omitted,
    /// Takes the width left.
    #[default]
    #[serde(other)]
    Text,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Align {
    Start,
    Center,
    End,
    #[default]
    Stretch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    Between,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TextStyle {
    #[default]
    Body,
    Small,
    /// Body text a size up, as a GitHub title.
    Large,
    Label,
    Title,
    Heading,
    Code,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tone {
    #[default]
    Default,
    Subtle,
    Accent,
    Success,
    Warning,
    Danger,
    /// A finished task's.
    Complete,
    /// A task in progress's.
    Progress,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ButtonStyle {
    Prominent,
    #[default]
    Subtle,
    Ghost,
    Outline,
    Destructive,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RingSegment {
    pub value: f32,
    #[serde(default)]
    pub tone: Tone,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuOption {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub icon_tone: Tone,
    /// A person's picture before the label.
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ChoiceOption {
    pub value: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AvatarSize {
    Small,
    #[default]
    Default,
    Large,
}

impl Node {
    /// Parses a `view/render` tree and checks its size.
    pub fn parse(value: Value) -> Result<Self, String> {
        let node: Self = serde_json::from_value(value)
            .map_err(|error| format!("The plugin sent a view that is not valid: {error}"))?;
        let mut count = 0;
        node.check(1, &mut count)?;
        Ok(node)
    }

    /// Every element directly inside this one.
    pub fn children(&self) -> Vec<&Node> {
        match self {
            Node::Column { children, .. }
            | Node::Row { children, .. }
            | Node::Card { children, .. }
            | Node::Header { children, .. }
            | Node::Scroll { children, .. } => children.iter().collect(),
            Node::Collapsible {
                marker,
                header,
                children,
                below,
                actions,
                footer,
                ..
            } => [marker, header, children, below, actions, footer]
                .into_iter()
                .flatten()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn check(&self, depth: usize, count: &mut usize) -> Result<(), String> {
        *count += 1;
        if *count > MAX_NODES {
            return Err(format!(
                "The plugin sent a view with over {MAX_NODES} elements."
            ));
        }
        if depth > MAX_DEPTH {
            return Err(format!(
                "The plugin sent a view nested over {MAX_DEPTH} deep."
            ));
        }
        self.children()
            .into_iter()
            .try_for_each(|child| child.check(depth + 1, count))
    }
}

/// A line from the plugin.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: u64,
        result: Result<Value, String>,
    },
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    message: String,
}

impl Incoming {
    pub fn parse(line: &str) -> Result<Self, String> {
        let raw: Raw =
            serde_json::from_str(line).map_err(|error| format!("not JSON-RPC: {error}"))?;
        let params = raw.params.unwrap_or(Value::Null);
        match (raw.method, raw.id) {
            (Some(method), Some(id)) => Ok(Self::Request { id, method, params }),
            (Some(method), None) => Ok(Self::Notification { method, params }),
            (None, Some(id)) => {
                let id = id.as_u64().ok_or("a response id the app did not send")?;
                let result = match raw.error {
                    Some(error) => Err(error.message),
                    None => Ok(raw.result.unwrap_or(Value::Null)),
                };
                Ok(Self::Response { id, result })
            }
            (None, None) => Err("a message with no method and no id".into()),
        }
    }
}

pub fn request(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

pub fn notification(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

pub fn response(id: &Value, result: Result<Value, (i64, String)>) -> String {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    }
    .to_string()
}

/// JSON-RPC's "method not found".
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC's "invalid params".
pub const INVALID_PARAMS: i64 = -32602;

#[cfg(test)]
mod tests;
