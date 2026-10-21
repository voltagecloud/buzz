//! The Buzz tools' client inside the app (buzz-app's
//! `src/buzz-mcp/app-client.ts`), shared by the Claude and Codex
//! types: the agent's host-held key signs and reads through its Agents2
//! handle, files are read by a declared process on that host, and memory is
//! read back through the owner, to whom it is encrypted.

use super::{BuzzClient, BuzzEvent, ClientResult, Memory, Template, Upload};
use crate::agent_plugins::process::{SpawnOptions, Spawner};
use crate::agents2::AgentHandle;
use base64::Engine;
use buzz_host::api::{AppAgentEvent, ProcessOutput};
use futures::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// The largest file a tool reads: the community's upload limit.
const READ_LIMIT: usize = 50 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// What the tools do as the agent: buzz-app's `AgentHandle`.
pub trait AgentActions: Send + Sync {
    fn pubkey(&self) -> String;
    fn owner(&self) -> String;
    /// Its display name, as Agents2 shows it.
    fn name(&self) -> String;
    fn publish(&self, event: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>>;
    fn query(&self, filters: Vec<Value>) -> BoxFuture<'static, ClientResult<Vec<BuzzEvent>>>;
    fn upload(&self, data: Vec<u8>, mime: String) -> BoxFuture<'static, ClientResult<Upload>>;
    /// One of its community's media blobs, as base64.
    fn media(&self, _url: String) -> BoxFuture<'static, ClientResult<String>> {
        Box::pin(async { Err("Buzz media is not available here".to_string()) })
    }
    fn remember(
        &self,
        slug: String,
        body: String,
        after: u64,
    ) -> BoxFuture<'static, ClientResult<()>>;
    /// Its own attention, for the attention tools and wakes.
    fn attention(&self) -> Option<Arc<dyn super::attention::Attention>> {
        None
    }
    /// Its owner's channel templates, where this app keeps them.
    fn templates(&self) -> Option<BoxFuture<'static, ClientResult<Vec<super::ChannelTemplate>>>> {
        None
    }
}

fn event(value: Value) -> ClientResult<BuzzEvent> {
    serde_json::from_value(value).map_err(|_| "Invalid agent event".to_string())
}

impl AgentActions for AgentHandle {
    fn pubkey(&self) -> String {
        self.pubkey.clone()
    }
    fn owner(&self) -> String {
        self.owner.clone()
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn attention(&self) -> Option<Arc<dyn super::attention::Attention>> {
        Some(Arc::new(self.attention.clone()))
    }
    fn templates(&self) -> Option<BoxFuture<'static, ClientResult<Vec<super::ChannelTemplate>>>> {
        let attention = self.attention.clone();
        Some(Box::pin(async move { attention.templates().await }))
    }
    fn publish(&self, template: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>> {
        let handle = self.clone();
        Box::pin(async move {
            let kind = u16::try_from(template.kind).map_err(|_| "Invalid event kind")?;
            event(
                handle
                    .publish(AppAgentEvent {
                        kind,
                        content: template.content,
                        tags: template.tags,
                    })
                    .await?,
            )
        })
    }
    fn query(&self, filters: Vec<Value>) -> BoxFuture<'static, ClientResult<Vec<BuzzEvent>>> {
        let handle = self.clone();
        Box::pin(async move {
            handle
                .query(&filters)
                .await?
                .into_iter()
                .map(event)
                .collect()
        })
    }
    fn upload(&self, data: Vec<u8>, mime: String) -> BoxFuture<'static, ClientResult<Upload>> {
        let handle = self.clone();
        Box::pin(async move {
            let receipt = handle.upload(&data, &mime).await?;
            serde_json::from_value(receipt).map_err(|_| "Invalid upload receipt".to_string())
        })
    }
    fn media(&self, url: String) -> BoxFuture<'static, ClientResult<String>> {
        let handle = self.clone();
        Box::pin(async move { handle.media(&url).await })
    }
    fn remember(
        &self,
        slug: String,
        body: String,
        after: u64,
    ) -> BoxFuture<'static, ClientResult<()>> {
        let handle = self.clone();
        Box::pin(async move { handle.remember(&slug, &body, after).await.map(drop) })
    }
}

/// Ends work in flight, with the reason it ended (buzz-app's
/// `AbortSignal`). The first reason wins. Cheap to clone; clones share it.
#[derive(Clone)]
pub struct Signal(Arc<watch::Sender<Option<String>>>);

impl Default for Signal {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(None)))
    }
}

impl Signal {
    pub fn abort(&self, reason: &str) {
        self.0.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason.into());
            true
        });
    }
    /// Why it was aborted, once it is.
    pub fn reason(&self) -> Option<String> {
        self.0.borrow().clone()
    }
    pub fn aborted(&self) -> bool {
        self.0.borrow().is_some()
    }
    /// `throwIfAborted`.
    pub fn check(&self) -> Result<(), String> {
        self.reason().map_or(Ok(()), Err)
    }
    /// Resolves with the reason once it is aborted.
    pub async fn wait(&self) -> String {
        let mut changes = self.0.subscribe();
        let reason = changes
            .wait_for(Option::is_some)
            .await
            .map(|reason| reason.clone().unwrap_or_default());
        match reason {
            Ok(reason) => reason,
            // The sender lives as long as this signal.
            Err(_) => std::future::pending().await,
        }
    }
}

pub type Memories = Arc<dyn Fn() -> BoxFuture<'static, ClientResult<Vec<Memory>>> + Send + Sync>;
/// Called after a memory write, with its slug.
pub type Remembered = Arc<dyn Fn(&str) + Send + Sync>;
/// Rejects operations after the owning turn or community has ended.
pub type Check = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// The tools' client for one agent.
pub struct AppClient {
    pub agent: Arc<dyn AgentActions>,
    pub spawn: Spawner,
    /// Where relative paths start: the agent's working directory.
    pub cwd: String,
    pub memories: Memories,
    pub remembered: Remembered,
    pub check: Option<Check>,
    /// Stops file reads in flight, and every later operation.
    pub signal: Option<Signal>,
}

impl AppClient {
    fn checked(&self) -> Result<(), String> {
        if let Some(signal) = &self.signal {
            signal.check()?;
        }
        self.check.as_ref().map_or(Ok(()), |check| check())
    }
}

fn refused<T: Send + 'static>(error: String) -> BoxFuture<'static, ClientResult<T>> {
    Box::pin(async move { Err(error) })
}

impl BuzzClient for AppClient {
    fn pubkey(&self) -> String {
        self.agent.pubkey()
    }
    fn query(&self, filters: Vec<Value>) -> BoxFuture<'static, ClientResult<Vec<BuzzEvent>>> {
        match self.checked() {
            Ok(()) => self.agent.query(filters),
            Err(error) => refused(error),
        }
    }
    fn publish(&self, event: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>> {
        match self.checked() {
            Ok(()) => self.agent.publish(event),
            Err(error) => refused(error),
        }
    }
    fn upload(&self, data: Vec<u8>, mime: String) -> BoxFuture<'static, ClientResult<Upload>> {
        match self.checked() {
            Ok(()) => self.agent.upload(data, mime),
            Err(error) => refused(error),
        }
    }
    fn media(&self, url: String) -> BoxFuture<'static, ClientResult<String>> {
        match self.checked() {
            Ok(()) => self.agent.media(url),
            Err(error) => refused(error),
        }
    }
    fn read(&self, path: String) -> BoxFuture<'static, ClientResult<Vec<u8>>> {
        if let Err(error) = self.checked() {
            return refused(error);
        }
        let (spawn, cwd, signal) = (self.spawn.clone(), self.cwd.clone(), self.signal.clone());
        Box::pin(async move { read_file(&spawn, &cwd, &path, signal.as_ref()).await })
    }
    fn memories(&self) -> BoxFuture<'static, ClientResult<Vec<Memory>>> {
        match self.checked() {
            Ok(()) => (self.memories)(),
            Err(error) => refused(error),
        }
    }
    fn remember(
        &self,
        slug: String,
        body: String,
        after: u64,
    ) -> BoxFuture<'static, ClientResult<()>> {
        if let Err(error) = self.checked() {
            return refused(error);
        }
        let write = self.agent.remember(slug.clone(), body, after);
        let remembered = self.remembered.clone();
        Box::pin(async move {
            write.await?;
            remembered(&slug);
            Ok(())
        })
    }
    fn templates(&self) -> Option<BoxFuture<'static, ClientResult<Vec<super::ChannelTemplate>>>> {
        let templates = self.agent.templates()?;
        Some(match self.checked() {
            Ok(()) => templates,
            Err(error) => refused(error),
        })
    }
}

/// Reads a file with the type's declared `read` process (`base64 -i`),
/// which passes binary files through intact. An aborted `signal` stops
/// the reader, even one that starts after it fired.
pub async fn read_file(
    spawn: &Spawner,
    cwd: &str,
    path: &str,
    signal: Option<&Signal>,
) -> ClientResult<Vec<u8>> {
    // `-` would wait on stdin, and anything else starting with `-` would be
    // read as an option. Like buzz-app, any other path is read.
    if path.trim().starts_with('-') || path.trim().is_empty() {
        return Err(format!("Cannot read {path}"));
    }
    let mut spawned = spawn
        .spawn(
            "read",
            SpawnOptions {
                args: vec![path.into()],
                cwd: Some(cwd.into()),
                env: Default::default(),
            },
        )
        .await?;
    let process = spawned.process.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(READ_TIMEOUT).await;
        let _ = process.kill().await;
    });
    let stop = signal.cloned().map(|signal| {
        let process = spawned.process.clone();
        tokio::spawn(async move {
            signal.wait().await;
            let _ = process.kill().await;
        })
    });
    let (mut output, mut error) = (String::new(), String::new());
    let mut large = false;
    let mut code = None;
    while let Some(item) = spawned.output.recv().await {
        match item {
            ProcessOutput::Stdout(text) => {
                output += &text;
                if output.len() > READ_LIMIT / 3 * 4 + 1024 && !large {
                    large = true;
                    let _ = spawned.process.kill().await;
                }
            }
            ProcessOutput::Stderr(text) => error += &text,
            ProcessOutput::Exit(exit) => {
                code = exit;
                break;
            }
        }
    }
    timer.abort();
    if let Some(stop) = stop {
        stop.abort();
    }
    if let Some(signal) = signal {
        signal.check()?;
    }
    if large {
        return Err(format!("{path} is larger than 50 MB"));
    }
    if code != Some(0) {
        let error = error.trim();
        return Err(if error.is_empty() {
            format!("Could not read {path}")
        } else {
            error.into()
        });
    }
    let text: String = output.split_whitespace().collect();
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| format!("Could not read {path}"))
}
