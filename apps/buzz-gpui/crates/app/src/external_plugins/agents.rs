//! Agent tools and agent types a plugin adds (buzz-app's `agents.addTool`
//! and `agents2.register`, `docs/plugins.md`).
//!
//! A tool call or an agent run is a request to the plugin, open until it
//! answers. Meanwhile the plugin can act as that agent with `agent/tool`,
//! which runs one of the Buzz tools (send, read, react, …) as the agent.
//! The agent's key stays on its host, as for every agent: the plugin never
//! sees it.

use super::process::Requester;
use super::protocol::{AgentTabRole, AgentToolDecl, AgentTypeDecl, INVALID_PARAMS};
use super::{ExternalPlugins, capabilities::Failure, contribute, intern};
use crate::agents2::Agent;
use crate::agents2::attention::{AttentionValue, EventData, Interest};
use crate::agents2::registry::{self, AgentTab, AgentType, TypeDefaults};
use crate::agents2::runtime::{Delivery, Trigger};
use crate::buzz_mcp::added::{self, AddedTool};
use crate::buzz_mcp::app_client::AgentActions;
use crate::buzz_mcp::tools::{Args, Reply, buzz_tool, call_tool};
use crate::buzz_mcp::{BuzzClient, BuzzEvent, ChannelTemplate, ClientResult, Context, Memory};
use crate::buzz_mcp::{Template, Upload};
use futures::FutureExt;
use futures::future::BoxFuture;
use gpui_kit::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long one tool call may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Who an open call or run acts as.
struct Acting {
    plugin: String,
    client: Arc<dyn BuzzClient>,
    context: Context,
    /// A run's agent's attention, for the attention tools; a tool call has
    /// none.
    attention: Option<Arc<dyn crate::buzz_mcp::attention::Attention>>,
    /// Whether a run's trigger is still current (`Delivery.current`).
    current: Option<crate::agents2::runtime::Current>,
    /// A run's local work row, for its steps and outcome.
    work: Option<crate::agent_activity::local::WorkHandle>,
}

static NEXT: AtomicU64 = AtomicU64::new(1);
static ACTING: Mutex<Option<HashMap<u64, Acting>>> = Mutex::new(None);

fn acting<R>(f: impl FnOnce(&mut HashMap<u64, Acting>) -> R) -> R {
    let mut acting = ACTING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(acting.get_or_insert_with(HashMap::new))
}

/// Holds `client` as call `id` until the returned guard drops.
fn open(
    plugin: &str,
    client: Arc<dyn BuzzClient>,
    context: Context,
    attention: Option<Arc<dyn crate::buzz_mcp::attention::Attention>>,
) -> (u64, impl Drop + use<>) {
    struct Close(u64);
    impl Drop for Close {
        fn drop(&mut self) {
            acting(|acting| acting.remove(&self.0));
        }
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    acting(|acting| {
        acting.insert(
            id,
            Acting {
                plugin: plugin.to_owned(),
                client,
                context,
                attention,
                current: None,
                work: None,
            },
        )
    });
    (id, Close(id))
}

/// A tool's answer: `{"text"}`, `{"image": {"data", "mime"}}` or a string.
fn reply(value: Value) -> Result<Reply, String> {
    if let Some(text) = value.as_str() {
        return Ok(Reply::Text(text.to_owned()));
    }
    if let Some(image) = value.get("image") {
        let field = |key: &str| image.get(key).and_then(Value::as_str).map(str::to_owned);
        return match (field("data"), field("mime")) {
            (Some(data), Some(mime)) if mime.starts_with("image/") => {
                Ok(Reply::Image { data, mime })
            }
            _ => Err("The plugin answered with an image that is not valid.".into()),
        };
    }
    Ok(Reply::Text(
        value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    ))
}

fn reply_json(reply: Reply) -> Value {
    match reply {
        Reply::Text(text) => json!({ "text": text }),
        Reply::Image { data, mime } => json!({ "image": { "data": data, "mime": mime } }),
    }
}

fn tool(plugin: &str, decl: &AgentToolDecl, requester: Requester) -> AddedTool {
    let (owner, name) = (plugin.to_owned(), decl.name.clone());
    AddedTool {
        owner: plugin.to_owned(),
        name: decl.name.clone(),
        description: decl.description.clone(),
        input_schema: decl.input_schema.clone(),
        call: Arc::new(move |client, context, args| {
            let (owner, name, requester) = (owner.clone(), name.clone(), requester.clone());
            async move {
                let agent = client.pubkey();
                let (call, _open) = open(&owner, client, context.clone(), None);
                let answered = requester
                    .request(
                        "agentTool/call",
                        json!({
                            "call": call,
                            "tool": name,
                            "arguments": Value::Object(args),
                            "agent": agent,
                            "channel": context.channel,
                            "root": context.root,
                        }),
                        TOOL_TIMEOUT,
                    )
                    .await;
                match answered {
                    Ok(Ok(value)) => reply(value),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.to_string()),
                }
            }
            .boxed()
        }),
    }
}

/// The Buzz tools' client for a plugin's agent: its handle, without the
/// local files and memories the harness runtimes add.
struct HandleClient(Arc<dyn AgentActions>);

fn refused<T: Send + 'static>(what: &str) -> BoxFuture<'static, ClientResult<T>> {
    let error = format!("{what} is not available to plugin agents");
    Box::pin(async move { Err(error) })
}

impl BuzzClient for HandleClient {
    fn pubkey(&self) -> String {
        self.0.pubkey()
    }
    fn query(&self, filters: Vec<Value>) -> BoxFuture<'static, ClientResult<Vec<BuzzEvent>>> {
        self.0.query(filters)
    }
    fn publish(&self, event: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>> {
        self.0.publish(event)
    }
    fn upload(&self, data: Vec<u8>, mime: String) -> BoxFuture<'static, ClientResult<Upload>> {
        self.0.upload(data, mime)
    }
    fn media(&self, url: String) -> BoxFuture<'static, ClientResult<String>> {
        self.0.media(url)
    }
    fn read(&self, _: String) -> BoxFuture<'static, ClientResult<Vec<u8>>> {
        refused("Reading local files")
    }
    fn memories(&self) -> BoxFuture<'static, ClientResult<Vec<Memory>>> {
        refused("Reading memories")
    }
    fn remember(
        &self,
        slug: String,
        body: String,
        after: u64,
    ) -> BoxFuture<'static, ClientResult<()>> {
        self.0.remember(slug, body, after)
    }
    fn templates(&self) -> Option<BoxFuture<'static, ClientResult<Vec<ChannelTemplate>>>> {
        self.0.templates()
    }
}

fn event_json(event: &EventData) -> Value {
    json!({
        "id": event.id,
        "pubkey": event.pubkey,
        "createdAt": event.created_at,
        "kind": event.kind,
        "content": event.content,
        "tags": event.tags,
    })
}

fn trigger_json(trigger: &Trigger) -> Value {
    match trigger {
        Trigger::Mention { event } => json!({ "kind": "mention", "event": event_json(event) }),
        Trigger::Watch {
            event,
            slug,
            watch,
            interest,
            classifier,
        } => json!({
            "kind": "watch",
            "event": event_json(event),
            "watch": slug,
            "watchObject": serde_json::to_value(watch).unwrap_or(Value::Null),
            "interest": interest.as_ref().map(|interest| interest.instructions.clone()),
            "classifierSkipped": classifier.as_ref().map(|skip| skip.reason.clone()),
        }),
        Trigger::Timer {
            slug,
            timer,
            interest,
            spent,
        } => json!({
            "kind": "timer",
            "timer": slug,
            "timerObject": serde_json::to_value(timer).unwrap_or(Value::Null),
            "interest": interest.as_ref().map(|interest| interest.instructions.clone()),
            "spent": spent,
        }),
    }
}

/// Where a run's tools default to: the event's channel and thread.
fn context(delivery: &Delivery) -> Context {
    let event = match &delivery.trigger {
        Trigger::Mention { event } | Trigger::Watch { event, .. } => Some(event),
        Trigger::Timer { .. } => None,
    };
    let root = event.map(|event| {
        event
            .tags
            .iter()
            .find(|tag| {
                tag.first().map(String::as_str) == Some("e")
                    && tag.get(3).map(String::as_str) == Some("root")
            })
            .and_then(|tag| tag.get(1).cloned())
            .unwrap_or_else(|| event.id.clone())
    });
    Context {
        channel: event
            .and_then(|event| event.tag("h").map(str::to_owned))
            .or_else(|| delivery.channel.clone()),
        root,
        top_level: false,
    }
}

struct PluginType {
    plugin: String,
    id: &'static str,
    title: SharedString,
    description: Option<SharedString>,
    config: Value,
    instructions: Option<String>,
    timeout: Duration,
    requester: Requester,
    summary: bool,
    peek: bool,
    /// Each tab's id, title, and whether it is the Settings tab.
    tabs: Vec<(String, String, bool)>,
    decl: AgentTypeDecl,
}

thread_local! {
        /// Each plugin provider's readiness, by type key, as it last said,
    /// and whether the plugin handles its attention itself.
    static READINESS: std::cell::RefCell<HashMap<String, (registry::ProviderSnapshot, bool)>> =
        std::cell::RefCell::default();
}

/// `agentType/readiness`: a plugin type's device-wide readiness.
pub fn set_readiness(plugin: &str, params: &Value, cx: &mut App) {
    use registry::{ProviderSnapshot, Readiness};
    let Some(kind) = params.get("type").and_then(Value::as_str) else {
        return;
    };
    let readiness = match params.get("readiness").and_then(Value::as_str) {
        Some("ready") => Readiness::Ready,
        Some("attention") => Readiness::Attention,
        _ => Readiness::Checking,
    };
    let snapshot = ProviderSnapshot {
        readiness,
        attention_label: params
            .get("attentionLabel")
            .and_then(Value::as_str)
            .map(|label| label.chars().take(64).collect()),
        blocking: params
            .get("blocking")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let action = params
        .get("action")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    READINESS.with_borrow_mut(|readiness| {
        readiness.insert(format!("{plugin}/{kind}"), (snapshot, action))
    });
    registry::providers_changed(cx);
}

/// A plugin type's provider: readiness it reports, a check it is asked
/// for, and its dialog as a plugin view.
struct Provider {
    plugin: String,
    kind: &'static str,
}

impl registry::AgentProvider for Provider {
    fn snapshot(&self, _: &App) -> registry::ProviderSnapshot {
        READINESS.with_borrow(|readiness| {
            readiness
                .get(&format!("{}/{}", self.plugin, self.kind))
                .map(|(snapshot, _)| snapshot.clone())
                .unwrap_or(registry::ProviderSnapshot::CHECKING)
        })
    }

    fn attention_action(&self, _: &App) -> Option<registry::AttentionAction> {
        let action = READINESS.with_borrow(|readiness| {
            readiness
                .get(&format!("{}/{}", self.plugin, self.kind))
                .is_some_and(|(_, action)| *action)
        });
        let (plugin, kind) = (self.plugin.clone(), self.kind);
        action.then(|| -> registry::AttentionAction {
            Box::new(move |cx| {
                ExternalPlugins::notify_running(
                    &plugin,
                    "agentType/attention",
                    json!({ "type": kind }),
                    cx,
                )
            })
        })
    }

    fn check(&self, cx: &mut App) {
        ExternalPlugins::notify_running(
            &self.plugin,
            "agentType/check",
            json!({ "type": self.kind }),
            cx,
        );
    }

    fn dialog_body(
        &self,
        close: registry::CloseProvider,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        super::view::ExternalView::agent_provider(&self.plugin, self.kind, close, window, cx)
    }
}

/// A hosted plugin type's provider: each step is a request to the plugin.
struct Hosted {
    requester: Requester,
    kind: &'static str,
}

/// How long one hosted step may take.
const HOSTED_TIMEOUT: Duration = Duration::from_secs(120);

impl Hosted {
    fn ask(&self, method: &str, mut params: Value, cx: &mut App) -> Task<Result<Value, String>> {
        params["type"] = json!(self.kind);
        let request = self.requester.request(method, params, HOSTED_TIMEOUT);
        cx.background_spawn(async move {
            match request.await {
                Ok(answer) => answer,
                Err(error) => Err(error.to_string()),
            }
        })
    }
}

impl registry::RemoteAgentType for Hosted {
    fn provision(&self, name: &str, community: &str, cx: &mut App) -> Task<Result<String, String>> {
        let asked = self.ask(
            "agent/provision",
            json!({ "name": name, "community": community }),
            cx,
        );
        cx.background_spawn(async move {
            asked
                .await?
                .get("pubkey")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .ok_or_else(|| "The provider did not return the agent's key".to_owned())
        })
    }

    fn activate(
        &self,
        pubkey: &str,
        name: &str,
        community: &str,
        auth: Vec<String>,
        cx: &mut App,
    ) -> Task<Result<(), String>> {
        let asked = self.ask(
            "agent/activate",
            json!({ "pubkey": pubkey, "name": name, "community": community, "auth": auth }),
            cx,
        );
        cx.background_spawn(async move { asked.await.map(drop) })
    }

    fn profile(
        &self,
        pubkey: &str,
        community: &str,
        name: &str,
        picture: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<(), String>> {
        let asked = self.ask(
            "agent/profile",
            json!({ "pubkey": pubkey, "community": community, "name": name, "picture": picture }),
            cx,
        );
        cx.background_spawn(async move { asked.await.map(drop) })
    }

    fn remove(&self, pubkey: &str, community: &str, cx: &mut App) -> Task<Result<(), String>> {
        let asked = self.ask(
            "agent/remove",
            json!({ "pubkey": pubkey, "community": community }),
            cx,
        );
        cx.background_spawn(async move { asked.await.map(drop) })
    }
}

/// A `agent/models` answer.
fn models_of(value: &Value) -> Option<registry::Models> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let models = value
        .get("models")?
        .as_array()?
        .iter()
        .take(100)
        .filter_map(|model| {
            Some(registry::Model {
                id: text(model, "id")?,
                label: text(model, "label").or_else(|| text(model, "id"))?,
                efforts: model
                    .get("efforts")
                    .and_then(Value::as_array)
                    .map(|efforts| {
                        efforts
                            .iter()
                            .take(16)
                            .filter_map(|effort| {
                                Some(registry::Effort {
                                    id: text(effort, "id")?,
                                    label: text(effort, "label").or_else(|| text(effort, "id"))?,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                effort: text(model, "effort"),
            })
        })
        .collect();
    Some(registry::Models {
        current: text(value, "current").unwrap_or_default(),
        models,
    })
}

/// An agent as a plugin sees it: the whole record, as `agents/list`
/// gives it (`AgentViewProps.agent`).
pub(super) fn agent_json(agent: &Agent) -> Value {
    entry_at(agent)
}

/// A summary answer, `{"text"}`, as a text tree for the answer cache.
fn summary_of(value: Value) -> Option<super::protocol::Node> {
    let text = value.get("text")?.as_str()?.trim();
    (!text.is_empty()).then(|| super::protocol::Node::Text {
        text: text.chars().take(120).collect(),
        style: Default::default(),
        tone: Default::default(),
        truncate: false,
        tooltip: None,
    })
}

impl PluginType {
    /// The answer cache's key for `what` (`summary` or `peek`), and its
    /// target: the agent as it is now, so a saved change asks again.
    fn cached(&self, what: &str, agent: &Agent) -> (&'static str, String) {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        agent_json(agent).to_string().hash(&mut hasher);
        (
            intern(format!("{}/{}#{what}", self.plugin, self.id)),
            format!("{}:{:x}", agent.pubkey, hasher.finish()),
        )
    }

    /// What the plugin answered about `agent`, asking it the first time.
    fn ask(
        &self,
        what: &str,
        agent: &Agent,
        parse: fn(Value) -> Option<super::protocol::Node>,
    ) -> Option<(&'static str, String, std::rc::Rc<super::protocol::Node>)> {
        let (key, target) = self.cached(what, agent);
        if contribute::due(key, &target) {
            contribute::ask_with(
                &self.requester,
                key,
                &target,
                &format!("agent/{what}"),
                json!({ "type": self.id, "agent": agent_json(agent) }),
                parse,
            );
        }
        contribute::answered(key, &target)
            .flatten()
            .map(|tree| (key, target, tree))
    }
}

impl AgentType for PluginType {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn description(&self) -> Option<SharedString> {
        self.description.clone()
    }

    fn defaults(&self) -> TypeDefaults {
        let mut attention: Vec<(String, AttentionValue)> = self
            .instructions
            .iter()
            .map(|instructions| {
                (
                    "interest/default".to_owned(),
                    AttentionValue::Interest(Interest {
                        instructions: instructions.clone(),
                    }),
                )
            })
            .collect();
        // Objects that do not parse are left out.
        attention.extend(self.decl.attention.iter().filter_map(|(slug, value)| {
            serde_json::from_value(value.clone())
                .ok()
                .map(|value| (slug.clone(), value))
        }));
        TypeDefaults {
            config: self.config.clone(),
            attention: attention.into_iter().collect(),
        }
    }

    fn summary(&self, agent: &Agent) -> Option<String> {
        if !self.summary {
            return None;
        }
        match &*self.ask("summary", agent, summary_of)?.2 {
            super::protocol::Node::Text { text, .. } => Some(text.clone()),
            _ => None,
        }
    }

    fn peek(&self, agent: &Agent, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        if !self.peek {
            return None;
        }
        let (key, target, tree) = self.ask("peek", agent, contribute::tree_of)?;
        Some(contribute::drawn(
            &self.plugin,
            self.id,
            key,
            &target,
            &tree,
            cx,
        ))
    }

    fn tabs(&self) -> Vec<AgentTab> {
        self.tabs
            .iter()
            .map(|(id, title, settings)| {
                let (plugin, kind, tab) = (self.plugin.clone(), self.id, id.clone());
                AgentTab {
                    // The app knows the Settings tab by this id.
                    id: if *settings {
                        "settings".into()
                    } else {
                        id.clone().into()
                    },
                    title: title.clone().into(),
                    build: Arc::new(move |view, window, cx| {
                        super::view::ExternalView::agent_tab(&plugin, kind, &tab, view, window, cx)
                    }),
                }
            })
            .collect()
    }

    fn runs(&self) -> bool {
        !self.decl.hosted
    }

    fn remote(&self) -> Option<std::rc::Rc<dyn registry::RemoteAgentType>> {
        self.decl.hosted.then(|| {
            std::rc::Rc::new(Hosted {
                requester: self.requester.clone(),
                kind: self.id,
            }) as std::rc::Rc<dyn registry::RemoteAgentType>
        })
    }

    fn create_instructions(&self) -> bool {
        self.decl.create_instructions
    }

    fn icon(&self) -> Option<buzz_ui::IconName> {
        self.decl
            .icon
            .as_deref()
            .map(|name| super::icon(Some(name)))
    }

    fn create_form(
        &self,
        config: &Value,
        change: registry::CreateChange,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyView> {
        self.decl.create_form.then(|| {
            super::view::ExternalView::agent_create(
                &self.plugin,
                self.id,
                config,
                change,
                window,
                cx,
            )
        })
    }

    fn create_models(&self, _: &mut App) -> Option<registry::CreateModels> {
        let decl = self.decl.create_models.as_ref()?;
        Some(registry::CreateModels {
            models: decl
                .models
                .iter()
                .map(|model| registry::ModelChoice {
                    id: model.id.clone(),
                    label: model.label.clone(),
                    description: model.description.clone(),
                })
                .collect(),
            default_label: decl
                .default_label
                .clone()
                .unwrap_or_else(|| "Default".to_owned()),
            default_description: None,
            allow_custom: decl.allow_custom,
            loading: false,
        })
    }

    fn provider(&self, _: &App) -> Option<std::rc::Rc<dyn registry::AgentProvider>> {
        self.decl.provider.then(|| {
            std::rc::Rc::new(Provider {
                plugin: self.plugin.clone(),
                kind: self.id,
            }) as std::rc::Rc<dyn registry::AgentProvider>
        })
    }

    fn models(
        &self,
        agent: &Agent,
        cx: &mut App,
    ) -> Option<Task<Result<registry::Models, String>>> {
        if !self.decl.models {
            return None;
        }
        let request = self.requester.request(
            "agent/models",
            json!({ "type": self.id, "agent": agent_json(agent) }),
            Duration::from_secs(30),
        );
        Some(cx.background_spawn(async move {
            match request.await {
                Ok(Ok(value)) => models_of(&value)
                    .ok_or_else(|| "The plugin's models were not valid.".to_owned()),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.to_string()),
            }
        }))
    }

    fn adopts(&self) -> bool {
        self.decl.carry.is_some()
    }

    fn adopt(&self, carry: &buzz_host::api::Carry, mut config: Value) -> Value {
        let Some(keys) = &self.decl.carry else {
            return config;
        };
        if let Value::Object(fields) = &mut config {
            for (key, value) in [
                (&keys.instructions, &carry.instructions),
                (&keys.workspace, &carry.workspace),
            ] {
                if let (Some(key), Some(value)) = (key, value) {
                    fields.insert(key.clone(), Value::from(value.clone()));
                }
            }
        }
        config
    }

    fn carry(&self, config: &Value) -> buzz_host::api::Carry {
        let Some(keys) = &self.decl.carry else {
            return buzz_host::api::Carry::default();
        };
        let read = |key: &Option<String>| {
            key.as_ref()
                .and_then(|key| config.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        buzz_host::api::Carry {
            instructions: read(&keys.instructions),
            workspace: read(&keys.workspace),
        }
    }

    fn migration_warning(&self) -> Option<&'static str> {
        self.decl.migration_warning.clone().map(intern)
    }

    fn describe_migration(&self, config: &Value) -> Vec<registry::MigrationSetting> {
        self.decl
            .migration_settings
            .iter()
            .map(|setting| {
                let value = config.get(&setting.key).filter(|value| !value.is_null());
                let is_override =
                    value.is_some_and(|value| Some(value) != setting.default.as_ref());
                registry::MigrationSetting {
                    id: intern(setting.id.clone()),
                    label: intern(setting.label.clone()),
                    is_override,
                    // The value in effect, override or not, as the built-in
                    // types key theirs (`unchangedKey`).
                    unchanged_key: value.or(setting.default.as_ref()).map(|value| {
                        value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_owned)
                    }),
                }
            })
            .collect()
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn run(&self, delivery: Delivery, cx: &mut App) -> Task<Result<(), String>> {
        let Some(requester) = ExternalPlugins::running_connection(&self.plugin, cx)
            .map(|connection| connection.requester())
        else {
            return Task::ready(Err("The plugin that runs this agent is not running.".into()));
        };
        let context = context(&delivery);
        let agent = &delivery.agent;
        let params_agent =
            json!({ "pubkey": agent.pubkey, "name": agent.name, "owner": agent.owner });
        let client: Arc<dyn BuzzClient> = Arc::new(HandleClient(Arc::new(agent.clone())));
        let attention = AgentActions::attention(agent);
        let (run, open) = open(&self.plugin, client, context.clone(), attention);
        let current = delivery.current.clone();
        let work = delivery.working.work();
        acting(|acting| {
            if let Some(open) = acting.get_mut(&run) {
                open.current = Some(current);
                open.work = work;
            }
        });
        let request = requester.request(
            "agent/run",
            json!({
                "run": run,
                "type": self.id,
                "trigger": trigger_json(&delivery.trigger),
                "channel": context.channel,
                "root": context.root,
                "agent": params_agent,
                "config": delivery.config,
                "model": delivery.model,
                "effort": delivery.effort,
                "team": delivery.team,
            }),
            self.timeout,
        );
        let cancel = delivery.cancel.clone();
        cx.background_spawn(async move {
            let _open = open;
            let cancelled = std::pin::pin!(cancel.cancelled());
            match futures::future::select(request, cancelled).await {
                futures::future::Either::Left((answered, _)) => match answered {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.to_string()),
                },
                futures::future::Either::Right(((), _)) => {
                    requester.notify("agent/cancel", json!({ "run": run }));
                    Err("The run was cancelled.".into())
                }
            }
        })
    }
}

/// What a plugin added to the agent registries.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AgentAdded {
    Tool(&'static str, &'static str),
    Type(&'static str),
}

pub fn take_away(added: AgentAdded, cx: &mut App) {
    match added {
        AgentAdded::Tool(owner, name) => added::remove(owner, name),
        AgentAdded::Type(key) => registry::unregister_type(key, cx),
    }
}

/// Registers the plugin's agent tools and types. An error names what was
/// refused; what passed stays registered.
pub fn register(
    plugin: &str,
    tools: &[AgentToolDecl],
    types: &[AgentTypeDecl],
    requester: Requester,
    cx: &mut App,
) -> (Vec<AgentAdded>, Vec<String>) {
    let (mut done, mut errors) = (Vec::new(), Vec::new());
    let owner = intern(plugin.to_owned());
    for decl in tools {
        match added::add(tool(plugin, decl, requester.clone())) {
            Ok(()) => done.push(AgentAdded::Tool(owner, intern(decl.name.clone()))),
            Err(error) => errors.push(error),
        }
    }
    for decl in types {
        let kind = PluginType {
            plugin: plugin.to_owned(),
            id: intern(decl.id.clone()),
            title: decl.title.clone().into(),
            description: decl.description.clone().map(Into::into),
            config: decl.config.clone().unwrap_or_else(|| json!({})),
            instructions: decl.instructions.clone(),
            timeout: Duration::from_secs(decl.timeout_seconds.unwrap_or(60)),
            requester: requester.clone(),
            summary: decl.summary,
            peek: decl.peek,
            tabs: decl
                .tabs
                .iter()
                .map(|tab| {
                    let settings = tab.role == Some(AgentTabRole::Settings);
                    (tab.id.clone(), tab.title.clone(), settings)
                })
                .collect(),
            decl: decl.clone(),
        };
        match registry::register_type(plugin, Arc::new(kind), cx) {
            Ok(()) => done.push(AgentAdded::Type(intern(format!("{plugin}/{}", decl.id)))),
            Err(error) => errors.push(error),
        }
    }
    (done, errors)
}

/// One of the agents this device keeps, for `agents/list` and the type
/// requests: `place` is `here`, whatever its type later became
/// (`service.ts` `view`).
fn entry_at(agent: &Agent) -> Value {
    let skipped: serde_json::Map<String, Value> = agent
        .skipped
        .iter()
        .map(|(slug, skipped)| {
            (
                slug.clone(),
                json!({
                    "value": skipped.value,
                    "modifiedAt": skipped.modified_at,
                    "problem": skipped.problem,
                }),
            )
        })
        .collect();
    json!({
        "pubkey": agent.pubkey,
        "name": agent.name,
        "type": agent.agent_type,
        "owner": agent.owner,
        "relay": agent.relay,
        "picture": agent.picture,
        "place": "here",
        "config": agent.config,
        "attentionEnabled": agent.attention_enabled,
        "attention": serde_json::to_value(&*agent.attention).unwrap_or(Value::Null),
        "skipped": skipped,
        "timers": serde_json::to_value(&*agent.timers).unwrap_or(Value::Null),
    })
}

pub(super) fn agents2(cx: &App) -> Result<Entity<crate::agents2::Agents2>, Failure> {
    crate::agents2::Agents2::global(cx).ok_or((1, "Agents are not available.".to_owned()))
}

fn https(value: Option<&Value>) -> Result<Option<String>, Failure> {
    match value {
        // `null` removes it, as `""` does (`AgentChange.picture`).
        Some(Value::Null) => return Ok(Some(String::new())),
        None => return Ok(None),
        Some(_) => {}
    }
    match value.and_then(Value::as_str) {
        None => Err((INVALID_PARAMS, "picture must be an https URL".to_owned())),
        Some("") => Ok(Some(String::new())),
        Some(url) if url.starts_with("https://") => Ok(Some(url.to_owned())),
        Some(_) => Err((INVALID_PARAMS, "picture must be an https URL".to_owned())),
    }
}

/// `agents/list`: the owner's agents in the selected community, and every
/// agent running on this device.
pub fn list(cx: &App) -> Result<Value, Failure> {
    let service = agents2(cx)?;
    let agents = service.read(cx);
    use crate::agents2::Status;
    let (status, error) = match agents.status() {
        Status::Unavailable => ("unavailable", None),
        Status::Loading => ("loading", None),
        Status::Ready => ("ready", None),
        Status::Error(error) => ("error", Some(error.clone())),
    };
    let viewer = match &cx.global::<crate::community::AppState>().identity {
        crate::community::IdentityState::Ready(identity) => Some(identity.pubkey_hex()),
        _ => None,
    };
    Ok(json!({
        "status": status,
        "error": error,
        "agents": agents.agents().iter().map(entry_at).collect::<Vec<_>>(),
        "running": agents.running().iter().map(entry_at).collect::<Vec<_>>(),
        "elsewhere": agents.elsewhere(cx).iter().map(|agent| {
            // A hosted agent runs where its host does, wherever it was
            // listed (`AgentPlace`).
            let hosted = crate::agents2::is_hosted(&agent.agent_type, cx);
            json!({
                "pubkey": agent.pubkey,
                "name": agent.name,
                "type": agent.agent_type,
                "owner": viewer,
                "relay": agent.relay,
                "picture": agent.picture,
                "place": if hosted { "hosted" } else { "elsewhere" },
                "hosted": hosted,
            })
        }).collect::<Vec<_>>(),
        "teams": agents.teams().iter().map(|team| json!({
            "id": team.id,
            "name": team.name,
            "instructions": team.instructions,
            "members": team.members,
        })).collect::<Vec<_>>(),
        "types": registry::types(cx).iter().map(|kind| json!({
            "type": kind.key,
            "title": kind.kind.title(),
            "description": kind.kind.description(),
            "hosted": kind.kind.remote().is_some(),
        })).collect::<Vec<_>>(),
    }))
}

/// `agents/choices`: the agents you may pick in the selected community
/// (`agentChoices`): `identities`, your agents its hosts run for this
/// relay, and `selectable`, those minus the ones the community archived,
/// with the archive read that decided it.
pub fn choices(cx: &mut App) -> Result<Value, Failure> {
    let community = super::capabilities::community(cx)?;
    crate::ui::archives::ensure(&community, false, cx);
    choices_now(cx)
}

/// [`choices`] as they stand, without asking for the archive.
pub(super) fn choices_now(cx: &App) -> Result<Value, Failure> {
    use crate::agents2::Status;
    use crate::ui::archives;
    let community = super::capabilities::community(cx)?;
    let origin = community.read(cx).origin.clone();
    // The hosts' agents (`native`).
    let (hosts, host_error) = match agents2(cx)?.read(cx).status() {
        Status::Unavailable => ("unavailable", None),
        Status::Loading => ("loading", None),
        Status::Ready => ("ready", None),
        Status::Error(error) => ("error", Some(error.clone())),
    };
    let mut seen = std::collections::HashSet::new();
    let mut identities: Vec<(String, String, bool)> =
        crate::ui::identity::AgentLibrary::from_hosts(cx)
            .agents
            .into_iter()
            .filter(|(_, _, relay)| {
                buzz_relay_client::RelayOrigin::parse(relay).is_ok_and(|other| other == origin)
            })
            .filter(|(key, _, _)| seen.insert(key.clone()))
            .map(|(key, name, _)| (key, name, true))
            .collect();
    // Each with its picture, as the library rows carry it (`avatar`).
    let service = agents2(cx)?;
    let service = service.read(cx);
    // That list is the `library`: either source being ready makes the
    // choices ready, and complete needs the list read and the hosts
    // settled (`createAgentChoices`).
    use crate::agents2::listings::ListingRead;
    let http = origin.http();
    let list = service.listing_state(&http);
    // And, while that list is read, the agents it names in this
    // community from other devices, which this one does not run
    // (`agentLibrary`'s relay inventory, unmanaged).
    if list == ListingRead::Ready {
        for (pubkey, name) in service.listed_elsewhere(&http) {
            if seen.insert(pubkey.clone()) {
                identities.push((pubkey, name, false));
            }
        }
    }
    let list_error = match &list {
        ListingRead::Error(failed) => Some(format!(
            "Relay inventory unavailable; showing local library ({failed})"
        )),
        _ => None,
    };
    let errors: Vec<String> = list_error.into_iter().chain(host_error).collect();
    let status = if list == ListingRead::Ready || hosts == "ready" {
        "ready"
    } else if !errors.is_empty() {
        "error"
    } else if list == ListingRead::Loading || hosts == "loading" {
        "loading"
    } else if list == ListingRead::Idle {
        "idle"
    } else {
        "unavailable"
    };
    let pending = matches!(list, ListingRead::Idle | ListingRead::Loading) || hosts == "loading";
    let complete = list == ListingRead::Ready && matches!(hosts, "ready" | "unavailable");
    let error = (!errors.is_empty()).then(|| errors.join(" "));
    let avatar = |pubkey: &str| {
        service
            .agents()
            .iter()
            .chain(service.running().iter())
            .find(|agent| agent.pubkey == pubkey)
            .and_then(|agent| agent.picture.clone())
            .filter(|picture| !picture.is_empty())
    };
    let choice = |(pubkey, name, managed): &(String, String, bool)| {
        json!({
            "pubkey": pubkey,
            "name": name,
            "avatar": avatar(pubkey),
            "managed": managed,
            "managedName": managed.then_some(name),
        })
    };
    Ok(json!({
        "status": status,
        "error": error,
        "pending": pending,
        "complete": complete,
        "identities": identities.iter().map(choice).collect::<Vec<_>>(),
        "selectable": identities
            .iter()
            .filter(|(pubkey, _, _)| !archives::hides(&community, pubkey, cx))
            .map(choice)
            .collect::<Vec<_>>(),
        "archives": archives::snapshot_json(&community, cx),
    }))
}

/// `agents/watch`: sends `agents/changed` with what `agents/list` gives,
/// now and whenever it changes.
pub fn watch(
    _params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(super::capabilities::Watch, Value), Failure> {
    let service = agents2(cx)?;
    let first = list(cx)?;
    let last = std::cell::RefCell::new(first.clone());
    let notify = std::rc::Rc::new(notify);
    let send = notify.clone();
    cx.defer(move |cx| send(first, cx));
    let subscription = cx.observe(&service, move |_, cx| {
        let Ok(now) = list(cx) else {
            return;
        };
        if now != *last.borrow() {
            *last.borrow_mut() = now.clone();
            notify(now, cx);
        }
    });
    Ok((
        super::capabilities::Watch::new(subscription, Task::ready(())),
        json!({}),
    ))
}

/// `teams/save` and `teams/remove`: the owner's agent teams.
pub fn team(method: &str, params: &Value, cx: &mut App) -> Result<Value, Failure> {
    let service = agents2(cx)?;
    if method == "teams/remove" {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "id is required".to_owned()))?
            .to_owned();
        return service
            .update(cx, |agents, cx| agents.remove_team(&id, cx))
            .map(|()| Value::Null)
            .map_err(|error| (3, error));
    }
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let change = crate::agents2::teams::TeamChange {
        id: text("id"),
        name: text("name").ok_or((INVALID_PARAMS, "name is required".to_owned()))?,
        instructions: text("instructions").unwrap_or_default(),
        members: params
            .get("members")
            .and_then(Value::as_array)
            .map(|members| {
                members
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    };
    let team = service
        .update(cx, |agents, cx| agents.save_team(change, cx))
        .map_err(|error| (3, error))?;
    Ok(
        json!({ "id": team.id, "name": team.name, "instructions": team.instructions, "members": team.members }),
    )
}

/// `agents/create`: a new agent of `type` (a `plugin/id` key) named
/// `name`, in the selected community.
pub fn create(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let text = |key: &str| params.get(key).and_then(Value::as_str);
    let kind = text("type").ok_or((INVALID_PARAMS, "type is required".to_owned()))?;
    if registry::type_of(kind, cx).is_none() {
        return Err((INVALID_PARAMS, format!("There is no agent type {kind}")));
    }
    let name = text("name").ok_or((INVALID_PARAMS, "name is required".to_owned()))?;
    let config = params
        .get("config")
        .filter(|config| !config.is_null())
        .cloned();
    if config.as_ref().is_some_and(|config| !config.is_object()) {
        return Err((INVALID_PARAMS, "config must be an object".to_owned()));
    }
    let made = crate::agents2::Made {
        config,
        picture: https(params.get("picture"))?,
        // What a shared agent brings, put where the type keeps it
        // (`port({ source: "shared" })`).
        carry: params
            .get("carry")
            .filter(|carry| carry.is_object())
            .map(|carry| {
                let field = |key: &str| {
                    carry
                        .get(key)
                        .and_then(Value::as_str)
                        .map(|text| text.chars().take(20_000).collect())
                };
                buzz_host::api::Carry {
                    instructions: field("instructions"),
                    workspace: field("workspace"),
                }
            }),
    };
    let (kind, name) = (kind.to_owned(), name.to_owned());
    let service = agents2(cx)?;
    // A hosted type's provider makes and runs it (`agent/provision`,
    // `agent/activate`), as the Agents page's create does.
    if crate::agents2::is_hosted(&kind, cx) {
        // No picture is none at all (`recordPicture`).
        let picture = made.picture.filter(|picture| !picture.is_empty());
        let task = service.update(cx, |agents, cx| {
            agents.create_hosted(&kind, &name, picture, cx)
        });
        return Ok(cx.spawn(async move |cx| {
            let pubkey = task.await.map_err(|error| (3, error))?;
            // As `agents/list` gives a hosted agent.
            Ok(cx.update(|cx| {
                let listed = service
                    .read(cx)
                    .elsewhere(cx)
                    .into_iter()
                    .find(|agent| agent.pubkey == pubkey);
                let viewer = crate::community::AppState::current(cx)
                    .map(|community| community.read(cx).session.viewer().to_owned());
                json!({
                    "pubkey": pubkey,
                    "name": listed.as_ref().map_or(name.as_str(), |agent| agent.name.as_str()),
                    "type": kind,
                    "owner": viewer,
                    "relay": listed.as_ref().map(|agent| agent.relay.clone()),
                    "picture": listed.as_ref().and_then(|agent| agent.picture.clone()),
                    "place": "hosted",
                    "hosted": true,
                })
            }))
        }));
    }
    let task = service.update(cx, |agents, cx| agents.create(&kind, &name, made, None, cx));
    Ok(cx.background_spawn(async move {
        task.await
            .map(|agent| entry_at(&agent))
            .map_err(|error| (3, error))
    }))
}

/// `agents/save`: changes an agent's name, picture, config or whether
/// its attention wakes it.
pub fn save(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let pubkey = params
        .get("pubkey")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "pubkey is required".to_owned()))?
        .to_owned();
    let config = params
        .get("config")
        .filter(|config| !config.is_null())
        .cloned();
    if config.as_ref().is_some_and(|config| !config.is_object()) {
        return Err((INVALID_PARAMS, "config must be an object".to_owned()));
    }
    let attention: Vec<(String, Option<Value>)> = params
        .get("attention")
        .and_then(Value::as_object)
        .map(|objects| {
            objects
                .iter()
                .map(|(slug, value)| (slug.clone(), (!value.is_null()).then(|| value.clone())))
                .collect()
        })
        .unwrap_or_default();
    let restart: Vec<String> = params
        .get("restart")
        .and_then(Value::as_array)
        .map(|slugs| {
            slugs
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let change = crate::agents2::AgentChange {
        name: params
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        picture: https(params.get("picture"))?,
        config,
        attention_enabled: params.get("attentionEnabled").and_then(Value::as_bool),
        attention,
        restart,
    };
    let service = agents2(cx)?;
    // A hosted agent's name and picture save through its provider.
    let hosted = service.update(cx, |agents, cx| {
        agents
            .elsewhere(cx)
            .iter()
            .any(|agent| agent.pubkey == pubkey && crate::agents2::is_hosted(&agent.agent_type, cx))
    });
    let task = service.update(cx, |agents, cx| match hosted {
        true => {
            let settings = change.config.is_some()
                || !change.attention.is_empty()
                || change.attention_enabled.is_some();
            agents.save_hosted(&pubkey, change.name, change.picture, settings, cx)
        }
        false => agents.save(&pubkey, change, cx),
    });
    Ok(cx.background_spawn(
        async move { task.await.map(|()| Value::Null).map_err(|error| (3, error)) },
    ))
}

/// `agents/remove`: deletes one of the owner's agents and its key.
pub fn remove(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let pubkey = params
        .get("pubkey")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "pubkey is required".to_owned()))?
        .to_owned();
    let service = agents2(cx)?;
    // A hosted agent is removed through its provider (`agent/remove`).
    let hosted = service.update(cx, |agents, cx| {
        agents
            .elsewhere(cx)
            .iter()
            .any(|agent| agent.pubkey == pubkey && crate::agents2::is_hosted(&agent.agent_type, cx))
    });
    let task = service.update(cx, |agents, cx| match hosted {
        true => agents.remove_hosted(&pubkey, cx),
        false => agents.remove(&pubkey, cx),
    });
    Ok(cx.background_spawn(
        async move { task.await.map(|()| Value::Null).map_err(|error| (3, error)) },
    ))
}

/// `agent/step`, `agent/acknowledged`, `agent/superseded` and
/// `agent/ignored`: what a run shows on its message (`Delivery.step`,
/// `acknowledged`, `superseded`, `ignored`).
pub fn presence(plugin: &str, method: &str, params: &Value) -> Result<Value, Failure> {
    let run = params
        .get("run")
        .and_then(Value::as_u64)
        .ok_or((INVALID_PARAMS, "run is required".to_owned()))?;
    let work = acting(|acting| {
        acting
            .get(&run)
            .filter(|open| open.plugin == plugin)
            .map(|open| open.work.clone())
    })
    .ok_or((1, "That run has ended.".to_owned()))?;
    // A run without a work row, such as a timer's, has nothing to show.
    let Some(work) = work else {
        return Ok(Value::Null);
    };
    match method {
        "agent/step" => {
            let tool = params
                .get("tool")
                .and_then(Value::as_str)
                .ok_or((INVALID_PARAMS, "tool is required".to_owned()))?;
            let target = params.get("target").and_then(Value::as_str);
            work.step(&tool.chars().take(64).collect::<String>(), target);
        }
        "agent/acknowledged" => work.started(),
        "agent/superseded" => work.supersede(),
        _ => work.remove(),
    }
    Ok(Value::Null)
}

/// `agents/activity`: what agents on this device are doing about which
/// messages, newest first.
pub fn activity(cx: &App) -> Value {
    let Some(local) = cx.try_global::<crate::agent_activity::local::LocalActivity>() else {
        return json!([]);
    };
    let mut work: Vec<_> = local.work().iter().cloned().collect();
    work.sort_by_key(|work| std::cmp::Reverse(work.started_at));
    // Each row's community, by its scope.
    let communities: Vec<(String, String)> = cx
        .global::<crate::community::AppState>()
        .communities
        .iter()
        .map(|community| {
            let community = community.read(cx);
            (
                crate::agent_activity::local::scope(
                    &community.origin.http(),
                    community.session.viewer(),
                ),
                community.origin.websocket().to_string(),
            )
        })
        .collect();
    Value::Array(
        work.iter()
            .map(|work| {
                json!({
                    "community": communities
                        .iter()
                        .find(|(scope, _)| *scope == work.scope)
                        .map(|(_, relay)| relay),
                    "dismissed": work.dismissed,
                    "agent": work.agent,
                    "channel": work.channel_id,
                    "message": work.message_id,
                    "root": work.thread_root_id,
                    "state": format!("{:?}", work.state).to_ascii_lowercase(),
                    "startedAt": work.started_at,
                    "error": work.error,
                    "step": work.step.as_ref().map(|step| json!({ "tool": step.tool, "target": step.target })),
                })
            })
            .collect(),
    )
}

/// `agents/watchActivity`: sends `agents/activityChanged` with what
/// `agents/activity` gives, now and whenever it changes.
pub fn watch_activity(
    _params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(super::capabilities::Watch, Value), Failure> {
    let Some(local) = cx.try_global::<crate::agent_activity::local::LocalActivity>() else {
        return Err((1, "No agents run on this device.".to_owned()));
    };
    let mut changes = local.subscribe();
    let forward = cx.spawn(async move |cx| {
        use futures::StreamExt;
        let mut last = Value::Null;
        loop {
            let now = cx.update(|cx| activity(cx));
            if now != last {
                cx.update(|cx| notify(json!({ "activity": now.clone() }), cx));
                last = now;
            }
            if changes.next().await.is_none() {
                return;
            }
        }
    });
    Ok((super::capabilities::Watch::new((), forward), json!({})))
}

/// `agent/current`: whether a run's trigger should still run, asked now
/// (`Delivery.current`). A run that queues work asks before starting it.
pub fn current(
    plugin: &str,
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    let run = params
        .get("run")
        .and_then(Value::as_u64)
        .ok_or((INVALID_PARAMS, "run is required".to_owned()))?;
    let current = acting(|acting| {
        acting
            .get(&run)
            .filter(|open| open.plugin == plugin)
            .map(|open| open.current.clone())
    })
    .ok_or((1, "That run has ended.".to_owned()))?
    .ok_or((INVALID_PARAMS, "agent/current takes a run".to_owned()))?;
    let asked = crate::runtime::Runtime::handle(cx).spawn(async move { current().await });
    Ok(cx.background_spawn(async move { Ok(json!(asked.await.unwrap_or(false))) }))
}

/// `agent/query`, `agent/publish` and `agent/upload`: raw reads, signed
/// writes and uploads as the agent of an open call or run
/// (`AgentHandle.query`, `publish`, `upload`).
pub fn act_raw(
    plugin: &str,
    method: &str,
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    use base64::Engine;
    let call = params
        .get("call")
        .or_else(|| params.get("run"))
        .and_then(Value::as_u64)
        .ok_or((INVALID_PARAMS, "call or run is required".to_owned()))?;
    let client = acting(|acting| {
        acting
            .get(&call)
            .filter(|open| open.plugin == plugin)
            .map(|open| open.client.clone())
    })
    .ok_or((1, "That call or run has ended.".to_owned()))?;
    let work: BoxFuture<'static, Result<Value, String>> = match method {
        "agent/query" => {
            let filters: Vec<Value> = params
                .get("filters")
                .and_then(Value::as_array)
                .filter(|filters| (1..=10).contains(&filters.len()))
                .ok_or((
                    INVALID_PARAMS,
                    "filters must list 1 to 10 filters".to_owned(),
                ))?
                .clone();
            let read = client.query(filters);
            Box::pin(async move {
                let events = read.await?;
                serde_json::to_value(events.into_iter().take(500).collect::<Vec<_>>())
                    .map_err(|error| error.to_string())
            })
        }
        "agent/publish" => {
            let kind = params
                .get("kind")
                .and_then(Value::as_u64)
                .and_then(|kind| u32::try_from(kind).ok())
                .ok_or((INVALID_PARAMS, "kind is required".to_owned()))?;
            let tags: Vec<Vec<String>> =
                serde_json::from_value(params.get("tags").cloned().unwrap_or_else(|| json!([])))
                    .map_err(|_| {
                        (
                            INVALID_PARAMS,
                            "tags must be a list of lists of strings".to_owned(),
                        )
                    })?;
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let write = client.publish(Template {
                kind,
                content,
                tags,
            });
            Box::pin(async move {
                serde_json::to_value(write.await?).map_err(|error| error.to_string())
            })
        }
        _ => {
            let mime = params
                .get("mime")
                .and_then(Value::as_str)
                .ok_or((INVALID_PARAMS, "mime is required".to_owned()))?
                .to_owned();
            // A file on disk up to the community's limit for its type, or
            // a small one inline.
            let data = match params.get("path").and_then(Value::as_str) {
                Some(path) => {
                    let limit = buzz_relay_client::messages::media_byte_limit(&mime);
                    std::fs::metadata(path)
                        .ok()
                        .filter(|meta| meta.is_file() && meta.len() > 0 && meta.len() <= limit)
                        .and_then(|_| std::fs::read(path).ok())
                        .ok_or((
                            INVALID_PARAMS,
                            "path must be a readable file within the community's limit".to_owned(),
                        ))?
                }
                None => params
                    .get("data")
                    .and_then(Value::as_str)
                    .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
                    .filter(|data| !data.is_empty() && data.len() <= 3 * 1024 * 1024)
                    .ok_or((
                        INVALID_PARAMS,
                        "data must be base64 of 1 byte to 3 MiB, or give a path".to_owned(),
                    ))?,
            };
            let upload = client.upload(data, mime);
            Box::pin(async move {
                serde_json::to_value(upload.await?).map_err(|error| error.to_string())
            })
        }
    };
    let done = crate::runtime::Runtime::handle(cx).spawn(work);
    Ok(cx.background_spawn(async move {
        match done.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err((2, error)),
            Err(error) => Err((2, error.to_string())),
        }
    }))
}

/// `agent/tool`: runs a Buzz tool as the agent of an open call or run.
pub fn act(
    plugin: &str,
    params: &Value,
    cx: &App,
) -> Result<Task<Result<Value, Failure>>, Failure> {
    let call = params
        .get("call")
        .or_else(|| params.get("run"))
        .and_then(Value::as_u64)
        .ok_or((INVALID_PARAMS, "call or run is required".to_owned()))?;
    let name = params
        .get("tool")
        .and_then(Value::as_str)
        .filter(|name| buzz_tool(name) || crate::buzz_mcp::attention::is_attention_tool(name))
        .ok_or((
            INVALID_PARAMS,
            "tool must be one of the Buzz tools".to_owned(),
        ))?
        .to_owned();
    let args: Args = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let (client, context, attention) = acting(|acting| {
        acting
            .get(&call)
            .filter(|open| open.plugin == plugin)
            .map(|open| {
                (
                    open.client.clone(),
                    open.context.clone(),
                    open.attention.clone(),
                )
            })
    })
    .ok_or((1, "That call or run has ended.".to_owned()))?;
    if crate::buzz_mcp::attention::is_attention_tool(&name) {
        // A run's agent manages its own watches and timers.
        let attention = attention.ok_or((
            INVALID_PARAMS,
            "Attention tools work only inside an agent run".to_owned(),
        ))?;
        let done = crate::runtime::Runtime::handle(cx).spawn(async move {
            if !attention.enabled().await? {
                return Err("The owner turned this agent's attention off.".to_owned());
            }
            crate::buzz_mcp::attention::call_attention_tool(&*attention, &name, &args).await
        });
        return Ok(cx.background_spawn(async move {
            match done.await {
                Ok(Ok(text)) => Ok(json!({ "text": text })),
                Ok(Err(error)) => Err((2, error)),
                Err(error) => Err((2, error.to_string())),
            }
        }));
    }
    let done = crate::runtime::Runtime::handle(cx)
        .spawn(async move { call_tool(client, &context, &name, &args).await });
    Ok(cx.background_spawn(async move {
        match done.await {
            Ok(Ok(reply)) => Ok(reply_json(reply)),
            Ok(Err(error)) => Err((2, error)),
            Err(error) => Err((2, error.to_string())),
        }
    }))
}

fn carry_key(key: &registry::CarryKey) -> &'static str {
    match key {
        registry::CarryKey::Instructions => "instructions",
        registry::CarryKey::Workspace => "workspace",
    }
}

/// `agents/dismissError`: hides a failed run's error on this device; its
/// outcome stays.
pub fn dismiss_error(params: &Value, cx: &App) -> Result<Value, Failure> {
    let field = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, format!("{key} is required")))
    };
    let (agent, message) = (field("agent")?, field("message")?);
    let Some(local) = cx.try_global::<crate::agent_activity::local::LocalActivity>() else {
        return Ok(Value::Null);
    };
    for work in local
        .work()
        .iter()
        .filter(|work| work.agent == agent && work.message_id == message)
    {
        local.dismiss_error(&work.scope, agent, message);
    }
    Ok(Value::Null)
}

/// `agents/clearActivity`: forgets what agents on this device did, of
/// one `agent` or all.
pub fn clear_activity(params: &Value, cx: &App) -> Result<Value, Failure> {
    if let Some(local) = cx.try_global::<crate::agent_activity::local::LocalActivity>() {
        local.clear(None, params.get("agent").and_then(Value::as_str));
    }
    Ok(Value::Null)
}

/// `agents/previewRetype`: what moving an agent on this device to another
/// type would do: the config it would start with, what it carries over
/// or leaves, and the settings the move resets or drops.
pub fn preview_retype(params: &Value, cx: &mut App) -> Result<Value, Failure> {
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, format!("{key} is required")))
    };
    let (pubkey, kind) = (text("pubkey")?.to_owned(), text("type")?.to_owned());
    let preview = agents2(cx)?
        .update(cx, |agents, cx| agents.preview_retype(&pubkey, &kind, cx))
        .map_err(|error| (3, error))?;
    Ok(json!({
        "config": preview.config,
        "moved": preview.moved.iter().map(carry_key).collect::<Vec<_>>(),
        "dropped": preview.dropped.iter().map(carry_key).collect::<Vec<_>>(),
        "settings": preview.settings.iter().map(|setting| json!({
            "id": setting.id,
            "label": setting.label,
            "outcome": format!("{:?}", setting.outcome).to_ascii_lowercase(),
        })).collect::<Vec<_>>(),
    }))
}

/// `agents/retype`: moves an agent on this device to another type, with
/// `config` (from `agents/previewRetype`, possibly edited) or the
/// type's defaults. Answers what stayed behind.
pub fn retype(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, format!("{key} is required")))
    };
    let (pubkey, kind) = (text("pubkey")?.to_owned(), text("type")?.to_owned());
    let config = params
        .get("config")
        .filter(|config| config.is_object())
        .cloned();
    let task = agents2(cx)?.update(cx, |agents, cx| agents.retype(&pubkey, &kind, config, cx));
    Ok(cx.background_spawn(async move {
        task.await
            .map(|dropped| json!({ "dropped": dropped.iter().map(carry_key).collect::<Vec<_>>() }))
            .map_err(|error| (3, error))
    }))
}

/// `agents/portable`: identities earlier agent systems saved on this
/// device that are not agents yet.
pub fn portable(cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let task = agents2(cx)?.update(cx, |agents, cx| agents.portable(cx));
    Ok(cx.background_spawn(async move {
        let found = task.await.map_err(|error| (3, error))?;
        Ok(json!({
            "agents": found.agents.iter().map(|portable| json!({
                "pubkey": portable.agent.pubkey,
                "name": portable.agent.name,
                "picture": portable.agent.picture,
                "relay": portable.agent.relay,
                "source": format!("{:?}", portable.agent.source).to_ascii_lowercase(),
            })).collect::<Vec<_>>(),
            "warnings": found.warnings,
        }))
    }))
}

/// `agents/port`: takes one of `agents/portable`'s identities over as an
/// agent of `type` in the selected community.
pub fn port(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, format!("{key} is required")))
    };
    let (pubkey, kind) = (text("pubkey")?.to_owned(), text("type")?.to_owned());
    let service = agents2(cx)?;
    let listed = service.update(cx, |agents, cx| agents.portable(cx));
    Ok(cx.spawn(async move |cx| {
        let found = listed.await.map_err(|error| (3, error))?;
        let portable = found
            .agents
            .into_iter()
            .find(|portable| portable.agent.pubkey == pubkey)
            .ok_or((3, "That identity cannot be brought over.".to_owned()))?;
        let task = service.update(cx, |agents, cx| agents.port(&portable, &kind, cx));
        task.await
            .map(|agent| entry_at(&agent))
            .map_err(|error| (3, error))
    }))
}

/// Handles plugins hold open with `agent/open`: by call id, the plugin
/// and the guard that keeps the call open.
type Opened = HashMap<u64, (String, Box<dyn std::any::Any + Send>)>;

static OPENED: Mutex<Option<Opened>> = Mutex::new(None);

fn opened<R>(f: impl FnOnce(&mut Opened) -> R) -> R {
    let mut opened = OPENED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(opened.get_or_insert_with(HashMap::new))
}

/// `agent/open`: acts as one of the plugin's agents outside any run, for
/// as long as the agent exists here (`AgentHandle`): the answer's `call`
/// works with `agent/tool`, `agent/publish`, `agent/query` and
/// `agent/upload` until `agent/close` or the plugin stops. `channel` and
/// `root` set where its tools default to.
pub fn open_agent(plugin: &str, params: &Value, cx: &App) -> Result<Value, Failure> {
    let pubkey = params
        .get("agent")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "agent (a pubkey) is required".to_owned()))?
        .to_ascii_lowercase();
    let service = agents2(cx)?;
    let service = service.read(cx);
    let prefix = format!("{plugin}/");
    // In any community: the selected one's, or running anywhere here.
    let ours = service
        .agents()
        .iter()
        .chain(service.running().iter())
        .any(|agent| agent.pubkey == pubkey && agent.agent_type.starts_with(&prefix));
    if !ours {
        return Err((
            INVALID_PARAMS,
            "agent is not one of this plugin's agents on this device".to_owned(),
        ));
    }
    let handle = service
        .handle_of(&pubkey, cx)
        .ok_or((3, "The agent does not run on this device now.".to_owned()))?;
    let attention = AgentActions::attention(&handle);
    let client: Arc<dyn BuzzClient> = Arc::new(HandleClient(Arc::new(handle)));
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let context = Context {
        channel: text("channel"),
        root: text("root"),
        top_level: false,
    };
    let (call, guard) = open(plugin, client, context, attention);
    opened(|opened| opened.insert(call, (plugin.to_owned(), Box::new(guard))));
    Ok(json!({ "call": call }))
}

/// `agent/close`: drops a handle from `agent/open`.
pub fn close_agent(plugin: &str, params: &Value) -> Result<Value, Failure> {
    let call = params
        .get("call")
        .and_then(Value::as_u64)
        .ok_or((INVALID_PARAMS, "call is required".to_owned()))?;
    opened(|opened| {
        if opened.get(&call).is_some_and(|(owner, _)| owner == plugin) {
            opened.remove(&call);
        }
    });
    Ok(Value::Null)
}

/// Drops every handle a plugin opened, as it stops.
pub fn close_all(plugin: &str) {
    opened(|opened| opened.retain(|_, (owner, _)| owner != plugin));
    // What a provider said holds only while its plugin runs.
    let prefix = format!("{plugin}/");
    READINESS.with_borrow_mut(|readiness| readiness.retain(|key, _| !key.starts_with(&prefix)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn plugin_models_are_read_and_bounded() {
        let models = models_of(&json!({
            "current": "a",
            "models": [
                { "id": "a", "label": "A", "efforts": [{ "id": "low" }, { "label": "no id" }], "effort": "low" },
                { "id": "b" },
                { "label": "no id" },
            ],
        }))
        .unwrap();
        assert_eq!(models.current, "a");
        assert_eq!(models.models.len(), 2);
        assert_eq!(models.models[0].efforts.len(), 1);
        assert_eq!(models.models[0].efforts[0].label, "low");
        assert_eq!(models.models[1].label, "b");
        assert!(models_of(&json!({})).is_none());
    }

    struct Nobody;

    impl BuzzClient for Nobody {
        fn pubkey(&self) -> String {
            "a".repeat(64)
        }
        fn query(&self, _: Vec<Value>) -> BoxFuture<'static, ClientResult<Vec<BuzzEvent>>> {
            refused("query")
        }
        fn publish(&self, _: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>> {
            refused("publish")
        }
        fn upload(&self, _: Vec<u8>, _: String) -> BoxFuture<'static, ClientResult<Upload>> {
            refused("upload")
        }
        fn read(&self, _: String) -> BoxFuture<'static, ClientResult<Vec<u8>>> {
            refused("read")
        }
        fn memories(&self) -> BoxFuture<'static, ClientResult<Vec<Memory>>> {
            refused("memories")
        }
        fn remember(&self, _: String, _: String, _: u64) -> BoxFuture<'static, ClientResult<()>> {
            refused("remember")
        }
    }

    #[test]
    fn replies_are_text_or_images() {
        assert_eq!(reply(json!("hi")), Ok(Reply::Text("hi".into())));
        assert_eq!(reply(json!({ "text": "ok" })), Ok(Reply::Text("ok".into())));
        assert_eq!(
            reply(json!({ "image": { "data": "AA==", "mime": "image/png" } })),
            Ok(Reply::Image {
                data: "AA==".into(),
                mime: "image/png".into()
            })
        );
        assert!(reply(json!({ "image": { "data": "AA==", "mime": "text/plain" } })).is_err());
    }

    #[test]
    fn open_calls_close_with_their_guard() {
        let (id, guard) = open("a.b", Arc::new(Nobody), Context::default(), None);
        assert!(acting(|acting| acting.contains_key(&id)));
        drop(guard);
        assert!(!acting(|acting| acting.contains_key(&id)));
    }

    /// The example plugin answers a tool call, and a Greeter run sends
    /// through `agent/tool` before it ends.
    #[test]
    fn the_example_plugin_runs_tools_and_agents() {
        use super::super::process::{Connection, Event};
        use super::super::protocol::{self, Contributions, Manifest};
        use futures::StreamExt;

        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped: no python3");
            return;
        }
        let folder =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/hello");
        let manifest =
            Manifest::parse(&std::fs::read_to_string(folder.join(protocol::MANIFEST)).unwrap())
                .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (connection, mut events) =
            Connection::start(&manifest, &folder, runtime.handle()).unwrap();
        runtime.block_on(async {
            let contributions = Contributions::parse(
                connection
                    .request("initialize", json!({ "apiVersion": 1 }))
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(contributions.agent_types[0].id, "greeter");

            let added = tool(
                "com.example.hello",
                &contributions.agent_tools[0],
                connection.requester(),
            );
            let answer = (added.call)(Arc::new(Nobody), Context::default(), Args::new())
                .await
                .unwrap();
            assert!(
                matches!(&answer, Reply::Text(text) if text.starts_with("It is ")),
                "{answer:?}"
            );

            let run = connection.requester().request(
                "agent/run",
                json!({
                    "run": 7,
                    "type": "greeter",
                    "trigger": { "kind": "mention", "event": { "content": "hi there" } },
                    "agent": { "pubkey": "a", "name": "Greeter", "owner": "b" },
                    "config": { "greeting": "Howdy" },
                }),
                Duration::from_secs(10),
            );
            let Some(Event::Request { id, method, params }) = events
                .by_ref()
                .filter(|event| {
                    let readiness = matches!(
                        event,
                        Event::Notification { method, .. } if method == "agentType/readiness"
                    );
                    std::future::ready(!readiness)
                })
                .next()
                .await
            else {
                panic!("no agent/tool");
            };
            assert_eq!(method, "agent/tool");
            assert_eq!(params["run"], 7);
            assert_eq!(params["tool"], "send");
            assert_eq!(
                params["arguments"]["text"],
                "Howdy from Greeter! You said: hi there"
            );
            connection.respond(&id, Ok(json!({ "text": "sent" })));
            assert_eq!(run.await.unwrap(), Ok(Value::Null));
        });
    }
}
