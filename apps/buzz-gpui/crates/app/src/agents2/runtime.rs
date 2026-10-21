//! What an agent type's `run` receives: the trigger, the agent's handle to
//! its custody host, its config and a cancellation signal
//! (buzz-app's `Delivery`, `AgentHandle` and `Trigger`).

// The type API: agent types compiled in elsewhere use what the app does not.
#![allow(dead_code)]

use super::attention::{EventData, EventWatch, Interest, TimerWatch};
use super::objects::{
    CLASSIFIER, ClassifierAvailability, Shown, WatchFilter, WatchSummary, WriteOptions,
};
use super::service::Agents2;
pub use super::working::Working;
use buzz_host::api::{
    AppAgentEvent, AppAgentRequest, AppAgentResponse, HostProtocol, Request, Response,
};
use buzz_host_rpc::{HostClient, RpcError};
use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::Shared;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Why an agent woke.
#[derive(Clone, Debug)]
pub enum Trigger {
    /// Directly addressed: a chat message that mentions the agent.
    Mention { event: Arc<EventData> },
    /// One matching watch; an event that matches several runs once for
    /// each.
    Watch {
        event: Arc<EventData>,
        slug: String,
        watch: EventWatch,
        /// `None` when the watch names an Interest that does not exist.
        interest: Option<Interest>,
        /// Set when the watch has a classifier that did not run. The
        /// event passes rather than lose a match to an unavailable model,
        /// and the agent is told why.
        classifier: Option<ClassifierSkip>,
    },
    Timer {
        slug: String,
        timer: TimerWatch,
        /// `None` when the timer names an Interest that does not exist.
        interest: Option<Interest>,
        /// This is its last allowed occurrence.
        spent: bool,
    },
}

/// Why a classified watch's event passed without a classifier answer
/// (outcome `not-run`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifierSkip {
    pub reason: String,
}

/// One run of an agent.
#[derive(Clone)]
pub struct Delivery {
    pub trigger: Trigger,
    /// The live route's channel; `None` when the wire scope was ambiguous.
    pub channel: Option<String>,
    pub agent: AgentHandle,
    /// The agent's config when this run started.
    pub config: Value,
    /// The model the owner asked for in the triggering message: one of the
    /// type's `models`. `None` means the agent's own model.
    pub model: Option<String>,
    /// The effort level the owner asked for in the triggering message: one
    /// of that model's efforts. `None` means the agent's own default.
    pub effort: Option<String>,
    /// A section for each team the agent is on, with the team's name,
    /// roster and instructions, to follow the agent's own instructions.
    /// `None` without one.
    pub team: Option<String>,
    /// Counts the run as working after `run` returns, for a type that
    /// hands its work off and returns early: see [`Working::until`].
    pub working: Working,
    /// Fires on the run's deadline, or when the agent is removed or its
    /// type is replaced. Advisory: the handle keeps working, so finishing
    /// up is harmless.
    pub cancel: Cancellation,
    /// Whether the trigger should still run, asked again now. False once
    /// the agent is removed or its type replaced; for a watch or timer,
    /// also once attention was turned off or its object disabled or
    /// removed; and for a watch, once it was changed or its event no
    /// longer matches. A type that queues a watch's or timer's work asks
    /// before it starts that work.
    pub current: Current,
}

/// Asks the Agents service whether a delivery's trigger is still current.
pub type Current = Arc<dyn Fn() -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

/// A [`Current`] for a run no service tracks: always current.
pub fn always_current() -> Current {
    Arc::new(|| Box::pin(async { true }))
}

/// A cancellation signal a run can poll or await.
#[derive(Clone)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
    fired: Shared<oneshot::Receiver<()>>,
}

impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled.
    pub async fn cancelled(&self) {
        let _ = self.fired.clone().await;
    }
}

/// The side that fires a [`Cancellation`]. Dropping it fires too.
pub(crate) struct CancelSource {
    flag: Arc<AtomicBool>,
    sender: Mutex<Option<oneshot::Sender<()>>>,
}

impl CancelSource {
    pub(crate) fn new() -> (Self, Cancellation) {
        let (sender, receiver) = oneshot::channel();
        let flag = Arc::new(AtomicBool::new(false));
        (
            Self {
                flag: flag.clone(),
                sender: Mutex::new(Some(sender)),
            },
            Cancellation {
                flag,
                fired: receiver.shared(),
            },
        )
    }

    pub(crate) fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Some(sender) = self.sender.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for CancelSource {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A host's RPC client and the runtime its requests run on.
#[derive(Clone)]
pub(crate) struct HostLink {
    pub client: Arc<HostClient<HostProtocol>>,
    pub runtime: tokio::runtime::Handle,
}

/// What fetching media through a host from before it answers.
pub const MEDIA_UNSUPPORTED: &str = "Update the agent's host to let it see Buzz images.";

/// What hosts from before Agents2 answer.
pub const UNSUPPORTED: &str = "This host does not support Agents yet; update it.";

impl HostLink {
    pub(crate) async fn app_agent(
        &self,
        request: AppAgentRequest,
    ) -> Result<AppAgentResponse, String> {
        let client = self.client.clone();
        let response = self
            .runtime
            .spawn(async move { client.request(Request::AppAgent(request)).await })
            .await
            .map_err(|_| "The request was cancelled".to_string())?;
        match response {
            Ok(Response::AppAgent(response)) => Ok(response),
            Ok(_) => Err("The host answered with something else".into()),
            Err(RpcError::Failed(message)) if message.contains("does not support this request") => {
                Err(UNSUPPORTED.into())
            }
            Err(RpcError::Failed(message)) => Err(message),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// The 2,048 most recent ids, as the spec asks for at least.
pub(crate) const SEEN_LIMIT: usize = 2_048;

/// A bounded set of event ids, oldest dropped first.
#[derive(Default)]
pub(crate) struct Bounded {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl Bounded {
    /// Adds `id`; false when it was already there.
    pub(crate) fn insert(&mut self, id: &str) -> bool {
        if !self.set.insert(id.to_owned()) {
            return false;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > SEEN_LIMIT
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }
}

/// Base64 text per staged upload part, under the RPC frame limit.
const UPLOAD_PART: usize = 8 * 1024 * 1024;

/// The agent as `run` holds it. It works for as long as the agent exists:
/// each call asks the agent's host to sign or read as the agent.
#[derive(Clone)]
pub struct AgentHandle {
    pub pubkey: String,
    pub name: String,
    pub owner: String,
    pub(crate) host: HostLink,
    /// Events the agent wrote, so reactions to them reach no watch.
    pub(crate) wrote: Arc<Mutex<Bounded>>,
    /// The agent's own attention, one object at a time.
    pub attention: AgentAttention,
}

/// Work for the Agents service, run on its thread.
pub(crate) type AttentionCall =
    Box<dyn FnOnce(&mut Agents2, &mut gpui_kit::Context<Agents2>) + Send>;

/// What an agent reads and writes of its attention (buzz-app's
/// `AgentAttention`). Every read is current, so a long run sees the
/// owner's switch and the classifier's state change. Each call is answered
/// by the Agents service, which keeps the records, so a write is checked
/// against the record as it is when it lands.
#[derive(Clone)]
pub struct AgentAttention {
    pub(crate) pubkey: String,
    pub(crate) calls: futures::channel::mpsc::UnboundedSender<AttentionCall>,
}

impl AgentAttention {
    /// One no service answers: every call fails.
    pub fn detached(pubkey: &str) -> Self {
        Self {
            pubkey: pubkey.into(),
            calls: futures::channel::mpsc::unbounded().0,
        }
    }

    /// Runs `call` on the service's thread, with the agent's pubkey.
    async fn ask<T: Send + 'static>(
        &self,
        call: impl FnOnce(&mut Agents2, &str, &mut gpui_kit::Context<Agents2>) -> T + Send + 'static,
    ) -> Result<T, String> {
        let (reply, answer) = oneshot::channel();
        let pubkey = self.pubkey.clone();
        self.calls
            .unbounded_send(Box::new(move |this, cx| {
                let _ = reply.send(call(this, &pubkey, cx));
            }))
            .map_err(|_| "Agents stopped".to_string())?;
        answer.await.map_err(|_| "Agents stopped".to_string())
    }

    /// False when the owner turned attention off: none of its objects
    /// wakes the agent. The handle still reads and writes them; the
    /// attention tools refuse every call while it is off.
    pub async fn enabled(&self) -> Result<bool, String> {
        self.ask(|this, pubkey, cx| this.attention_enabled(pubkey, cx))
            .await?
    }

    /// The owner's channel templates, each with the agents it adds.
    pub async fn templates(&self) -> Result<Vec<crate::buzz_mcp::ChannelTemplate>, String> {
        self.ask(|this, pubkey, cx| this.channel_templates(pubkey, cx))
            .await
    }

    /// Whether a watch's classifier can run now. Classifiers can be saved
    /// either way; one that cannot run lets every event pass.
    pub fn classifier(&self) -> ClassifierAvailability {
        CLASSIFIER
    }

    /// One object and its expected-state token. `interest` scopes the
    /// read: an object of another Interest is `wrong-scope`.
    pub async fn show(&self, slug: &str, interest: Option<&str>) -> Result<Shown, String> {
        let (slug, interest) = (slug.to_owned(), interest.map(str::to_owned));
        self.ask(move |this, pubkey, cx| {
            this.attention_show(pubkey, &slug, interest.as_deref(), cx)
        })
        .await?
    }

    pub async fn interests(&self, search: &str) -> Result<Vec<String>, String> {
        let search = search.to_owned();
        self.ask(move |this, pubkey, cx| this.attention_interests(pubkey, &search, cx))
            .await?
    }

    pub async fn watches(&self, filter: WatchFilter) -> Result<Vec<WatchSummary>, String> {
        self.ask(move |this, pubkey, cx| this.attention_watches(pubkey, &filter, cx))
            .await?
    }

    /// Replaces, or with `None` removes, one object; returns it as shown
    /// after the write. A refused write is an `AttentionError`'s text.
    pub async fn write(
        &self,
        slug: &str,
        value: Option<Value>,
        options: WriteOptions,
    ) -> Result<Shown, String> {
        let slug = slug.to_owned();
        self.ask(move |this, pubkey, cx| {
            this.attention_write(pubkey, &slug, value.as_ref(), &options, cx)
        })
        .await?
    }
}

impl AgentHandle {
    /// Signs one event as the agent (deletions, reactions, messages, edits,
    /// canvases and DM opens only) and posts it to its community. Returns
    /// the signed event.
    pub async fn publish(&self, event: AppAgentEvent) -> Result<Value, String> {
        let signed = self
            .event(AppAgentRequest::Publish {
                pubkey: self.pubkey.clone(),
                event,
            })
            .await?;
        if let Some(id) = signed.get("id").and_then(Value::as_str) {
            self.wrote
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id);
        }
        Ok(signed)
    }

    /// Signs and posts one event as the agent without counting it among
    /// what it wrote, such as a typing pulse.
    pub(crate) async fn signal(&self, event: AppAgentEvent) -> Result<(), String> {
        self.event(AppAgentRequest::Publish {
            pubkey: self.pubkey.clone(),
            event,
        })
        .await
        .map(drop)
    }

    /// Reads the community as the agent, seeing only what it may see: one
    /// to eight filters.
    pub async fn query(&self, filters: &[Value]) -> Result<Vec<Value>, String> {
        let filters = serde_json::to_string(filters).map_err(|e| e.to_string())?;
        match self
            .host
            .app_agent(AppAgentRequest::Query {
                pubkey: self.pubkey.clone(),
                filters,
            })
            .await?
        {
            AppAgentResponse::Events(json) => {
                serde_json::from_str(&json).map_err(|_| "Invalid agent read".into())
            }
            _ => Err("Invalid agent read".into()),
        }
    }

    /// Uploads `data`, an image or MP4 video, as the agent. Returns the
    /// community's description of it (`url`, `sha256`, `size`, `type`...).
    pub async fn upload(&self, data: &[u8], mime: &str) -> Result<Value, String> {
        use base64::Engine;
        let text = base64::engine::general_purpose::STANDARD.encode(data);
        let mut parts: Vec<&str> = Vec::new();
        let mut rest = text.as_str();
        while rest.len() > UPLOAD_PART {
            let (part, tail) = rest.split_at(UPLOAD_PART);
            parts.push(part);
            rest = tail;
        }
        let staged = (!parts.is_empty()).then(|| uuid::Uuid::new_v4().to_string());
        for part in parts {
            self.host
                .app_agent(AppAgentRequest::UploadPart {
                    staged: staged.clone().expect("staged when parted"),
                    data: part.into(),
                })
                .await?;
        }
        match self
            .host
            .app_agent(AppAgentRequest::Upload {
                pubkey: self.pubkey.clone(),
                data: rest.into(),
                mime: mime.into(),
                staged,
            })
            .await?
        {
            AppAgentResponse::Uploaded(json) => {
                serde_json::from_str(&json).map_err(|_| "Invalid upload receipt".into())
            }
            _ => Err("Invalid upload receipt".into()),
        }
    }

    /// Downloads one of its community's media blobs, by URL or
    /// `sha256[.ext]`, as the agent: base64.
    pub async fn media(&self, url: &str) -> Result<String, String> {
        match self
            .host
            .app_agent(AppAgentRequest::Media {
                pubkey: self.pubkey.clone(),
                url: url.into(),
            })
            .await
        {
            Ok(AppAgentResponse::Media(data)) => Ok(data),
            Ok(_) => Err("Invalid agent media".into()),
            Err(error) if error == UNSUPPORTED => Err(MEDIA_UNSUPPORTED.into()),
            Err(error) => Err(error),
        }
    }

    /// Writes memory entry `slug`, newer than the entry it replaces
    /// (`after`, that entry's `created_at`, or 0). The owner reads it back.
    pub async fn remember(&self, slug: &str, body: &str, after: u64) -> Result<Value, String> {
        self.event(AppAgentRequest::Remember {
            pubkey: self.pubkey.clone(),
            slug: slug.into(),
            body: body.into(),
            after,
        })
        .await
    }

    async fn event(&self, request: AppAgentRequest) -> Result<Value, String> {
        match self.host.app_agent(request).await? {
            AppAgentResponse::Event(json) => {
                serde_json::from_str(&json).map_err(|_| "Invalid agent event".into())
            }
            _ => Err("Invalid agent event".into()),
        }
    }
}
