//! Terminal sessions (`sessions.ts`): one login shell per channel,
//! kept while its panel is hidden, at most twenty at a time.
//!
//! The shell runs on an agent host, through the host protocol: the
//! embedded host for "This computer", or a remote buzzd. Output is pulled
//! with long-poll reads; input is written in order, at most 1 MiB queued.

use super::emulator::{Screen, Theme};
use crate::community::{AppState, Community};
use crate::hosts::{HostConnection, Hosts};
use crate::runtime::Runtime;
use buzz_host::api::{HostProtocol, Request, Response, TerminalContext, TerminalOutput};
use buzz_host_rpc::{HostClient, RpcError};
use buzz_relay_client::SessionStatus;
use buzz_ui::ActiveTheme;
use futures::future::Shared;
use futures::{FutureExt, SinkExt, StreamExt};
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Open sessions across every channel and community.
pub const MAX_SESSIONS: usize = 20;
/// Input queued for one shell before it is refused (`pendingBytes`).
const MAX_PENDING_INPUT: usize = 1024 * 1024;
/// How long one read waits on the host for output.
const READ_WAIT_MS: u32 = 1000;
/// The size a shell starts at, before its panel measures itself.
const START_SIZE: (u16, u16) = (80, 24);

pub const TOO_MANY: &str =
    "Error: Twenty terminal sessions are open. End one before opening another.";
pub const INPUT_FULL: &str = "Terminal input is full. End this session and start again.";
pub const OFFLINE: &str =
    "Buzz can’t reach this host. Check that it is running, then restart the terminal.";
pub const UNSUPPORTED: &str =
    "This host does not support terminals. Update Buzz on that machine to use one here.";

/// Where a terminal was opened (`ChannelPanelContext`).
#[derive(Clone, Debug)]
pub struct PanelContext {
    pub community: WeakEntity<Community>,
    /// The community and who is signed in to it, which sessions are
    /// kept apart by.
    pub scope: String,
    pub viewer: String,
    pub channel_id: String,
    pub channel_name: String,
    /// The thread showing beside the channel, if any.
    pub thread_id: Option<String>,
    /// The community's relay, credential-free.
    pub relay_url: String,
}

impl PanelContext {
    pub fn new(
        community: &Entity<Community>,
        channel_id: String,
        channel_name: String,
        thread_id: Option<String>,
        cx: &App,
    ) -> Self {
        let state = community.read(cx);
        let viewer = state.session.viewer().to_string();
        let relay_url = state.origin.websocket().to_string();
        Self {
            community: community.downgrade(),
            scope: format!("{relay_url}/{viewer}"),
            viewer,
            channel_id,
            channel_name,
            thread_id,
            relay_url,
        }
    }

    pub fn key(&self) -> Key {
        Key {
            scope: self.scope.clone(),
            channel: self.channel_id.clone(),
        }
    }

    /// What the shell is told (`ShellContext`).
    fn shell(&self) -> Result<TerminalContext, String> {
        use nostr::nips::nip19::ToBech32;
        let npub = nostr::key::PublicKey::from_hex(&self.viewer)
            .ok()
            .and_then(|key| key.to_bech32().ok())
            .ok_or("Invalid terminal public npub")?;
        Ok(TerminalContext {
            channel_id: self.channel_id.clone(),
            channel_name: self.channel_name.clone(),
            thread_id: self.thread_id.clone(),
            npub,
            relay_url: self.relay_url.clone(),
        })
    }
}

/// Sessions are kept per community, viewer and channel.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    scope: String,
    channel: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Starting,
    Running,
    Exited,
    Error,
    Ended,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Error => "error",
            Self::Ended => "ended",
        }
    }
}

/// Who typed: the person, or the emulator answering the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    User,
    Reply,
}

pub struct Session {
    /// Captured when the shell started.
    pub context: PanelContext,
    pub status: Status,
    pub error: Option<String>,
    pub screen: Option<Entity<Screen>>,
    /// The host the shell runs on.
    pub host: EntityId,
    serial: u64,
    owner: Option<String>,
    id: Option<String>,
    io: Option<Io>,
}

/// A running shell's traffic.
struct Io {
    writes: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<AtomicUsize>,
    size: tokio::sync::watch::Sender<(u16, u16)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _events: Task<()>,
}

impl Drop for Io {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

enum IoEvent {
    Output(TerminalOutput),
    Failed(String),
}

type OwnerTask = Shared<Task<Result<String, String>>>;

/// Every terminal session; lives as long as the app, not as any panel.
#[derive(Default)]
pub struct Sessions {
    sessions: HashMap<Key, Session>,
    /// Channels that had a session: showing their panel again is not a
    /// request to start a shell the person ended.
    initialized: HashSet<Key>,
    /// One owner per host, made on first use.
    owners: HashMap<EntityId, OwnerTask>,
    /// The host chosen for a channel's next shell.
    chosen: HashMap<Key, EntityId>,
    /// Pixel size of one cell, for size queries.
    pub cell: (u16, u16),
    next_serial: u64,
}

struct Global(Entity<Sessions>);

impl gpui_kit::Global for Global {}

impl Sessions {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<Global>() {
            return global.0.clone();
        }
        let sessions = cx.new(|_| Self {
            cell: (7, 18),
            ..Default::default()
        });
        cx.set_global(Global(sessions.clone()));
        sessions
    }

    /// Whether this app can start terminals at all: some host runs here
    /// or is configured.
    pub fn available(cx: &App) -> bool {
        cx.try_global::<Hosts>()
            .is_some_and(|hosts| !hosts.hosts.is_empty())
    }

    pub fn get(&self, key: &Key) -> Option<&Session> {
        self.sessions.get(key)
    }

    /// The hosts a shell can run on, this computer first.
    pub fn hosts(cx: &App) -> Vec<Entity<HostConnection>> {
        let Some(hosts) = cx.try_global::<Hosts>() else {
            return Vec::new();
        };
        let mut list = hosts.hosts.clone();
        list.sort_by_key(|host| host.read(cx).remote.is_some());
        list
    }

    /// The host a channel's next shell starts on: the one picked for it,
    /// else this computer.
    pub fn host_for(&self, key: &Key, cx: &App) -> Option<Entity<HostConnection>> {
        let hosts = Self::hosts(cx);
        let chosen = self.chosen.get(key);
        hosts
            .iter()
            .find(|host| Some(&host.entity_id()) == chosen)
            .or_else(|| hosts.first())
            .cloned()
    }

    pub fn choose_host(&mut self, key: Key, host: EntityId, cx: &mut Context<Self>) {
        self.chosen.insert(key, host);
        cx.notify();
    }

    /// New input belongs to the community on screen while it is connected
    /// (`allowed`).
    fn allowed(context: &PanelContext, cx: &App) -> bool {
        let Some(community) = context.community.upgrade() else {
            return false;
        };
        let state = community.read(cx);
        state.snapshot.status == SessionStatus::Ready
            && state.session.viewer() == context.viewer
            && AppState::current(cx).is_some_and(|current| current == community)
    }

    fn live(&self, key: &Key, serial: u64) -> bool {
        self.sessions
            .get(key)
            .is_some_and(|session| session.serial == serial && session.status != Status::Ended)
    }

    /// Starts the channel's shell unless it has one. With `initial`, a
    /// channel whose shell was ended stays ended (`ensure`).
    pub fn ensure(
        &mut self,
        context: &PanelContext,
        initial: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let key = context.key();
        if self.sessions.contains_key(&key) || (initial && self.initialized.contains(&key)) {
            return Ok(());
        }
        if !Self::available(cx) || !Self::allowed(context, cx) {
            return Ok(());
        }
        admit(self.sessions.len())?;
        let Some(host) = self.host_for(&key, cx) else {
            return Ok(());
        };
        self.next_serial += 1;
        let serial = self.next_serial;
        self.sessions.insert(
            key.clone(),
            Session {
                context: context.clone(),
                status: Status::Starting,
                error: None,
                screen: None,
                host: host.entity_id(),
                serial,
                owner: None,
                id: None,
                io: None,
            },
        );
        self.initialized.insert(key.clone());
        cx.notify();
        self.start(key, serial, host, cx);
        Ok(())
    }

    /// The host's owner for this app's terminals, made once.
    fn owner(&mut self, host: &Entity<HostConnection>, cx: &mut Context<Self>) -> OwnerTask {
        if let Some(owner) = self.owners.get(&host.entity_id()) {
            return owner.clone();
        }
        let client = host.read(cx).client();
        let runtime = Runtime::handle(cx);
        let task = cx
            .background_spawn(async move {
                let response = runtime
                    .spawn(async move { client.request(Request::TerminalOwner).await })
                    .await
                    .unwrap_or(Err(RpcError::Disconnected));
                match response {
                    Ok(Response::TerminalOwner(owner)) => Ok(owner),
                    Ok(_) => Err("Unexpected terminal response".to_string()),
                    Err(error) => Err(message(error)),
                }
            })
            .shared();
        self.owners.insert(host.entity_id(), task.clone());
        task
    }

    fn forget_owner(&mut self, host: EntityId, owner: &str) {
        let stale = self
            .owners
            .get(&host)
            .and_then(|task| task.peek().cloned())
            .is_some_and(|result| result.as_deref() == Ok(owner) || result.is_err());
        if stale {
            self.owners.remove(&host);
        }
    }

    fn start(
        &mut self,
        key: Key,
        serial: u64,
        host: Entity<HostConnection>,
        cx: &mut Context<Self>,
    ) {
        let client = host.read(cx).client();
        let host_id = host.entity_id();
        cx.spawn(async move |this, cx| {
            // A cached owner may have closed with an earlier connection;
            // one retry gets a fresh one.
            for attempt in 0..2 {
                let Ok(owner) = this.update(cx, |this, cx| this.owner(&host, cx)) else {
                    return;
                };
                let owner = match owner.await {
                    Ok(owner) => owner,
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| {
                            this.owners.remove(&host_id);
                            this.fail(&key, serial, error, cx);
                        });
                        return;
                    }
                };
                let Ok(Some(shell)) = this.update(cx, |this, cx| {
                    let session = this.sessions.get_mut(&key)?;
                    let context = session.context.clone();
                    if !this.live(&key, serial) || !Self::allowed(&context, cx) {
                        this.retire(&key, serial, cx);
                        return None;
                    }
                    match context.shell() {
                        Ok(shell) => Some(shell),
                        Err(error) => {
                            this.fail(&key, serial, error, cx);
                            None
                        }
                    }
                }) else {
                    return;
                };
                let response = request(
                    &client,
                    Request::TerminalSpawn {
                        owner: owner.clone(),
                        context: shell,
                        cols: START_SIZE.0,
                        rows: START_SIZE.1,
                    },
                    cx,
                )
                .await;
                let id = match response {
                    Ok(Response::TerminalSession(id)) => id,
                    Ok(_) => {
                        let _ = this.update(cx, |this, cx| {
                            this.fail(&key, serial, "Unexpected terminal response".into(), cx)
                        });
                        return;
                    }
                    Err(error) => {
                        let closed = matches!(&error, RpcError::Failed(text) if text == "Terminal owner has closed");
                        let _ = this.update(cx, |this, cx| {
                            this.forget_owner(host_id, &owner);
                            if !(closed && attempt == 0) {
                                this.fail(&key, serial, message(error), cx);
                            }
                        });
                        if closed && attempt == 0 {
                            continue;
                        }
                        return;
                    }
                };
                let _ = this.update(cx, |this, cx| {
                    this.started(key.clone(), serial, client.clone(), owner, id, cx)
                });
                return;
            }
        })
        .detach();
    }

    /// The shell is up: make its screen and start its traffic, unless the
    /// session ended meanwhile.
    fn started(
        &mut self,
        key: Key,
        serial: u64,
        client: Arc<HostClient<HostProtocol>>,
        owner: String,
        id: String,
        cx: &mut Context<Self>,
    ) {
        let allowed = self
            .sessions
            .get(&key)
            .is_some_and(|session| Self::allowed(&session.context, cx));
        if !self.live(&key, serial) || !allowed {
            close_later(client, owner, id, cx);
            self.retire(&key, serial, cx);
            return;
        }
        let runtime = Runtime::handle(cx);
        let (events, mut received) = futures::channel::mpsc::channel::<IoEvent>(4);
        let mut tasks = Vec::new();
        // Output, one read in flight, so it arrives in order.
        {
            let (client, owner, id) = (client.clone(), owner.clone(), id.clone());
            let mut events = events.clone();
            tasks.push(runtime.spawn(async move {
                loop {
                    let read = client
                        .request(Request::TerminalRead {
                            owner: owner.clone(),
                            id: id.clone(),
                            wait_ms: READ_WAIT_MS,
                        })
                        .await;
                    let event = match read {
                        Ok(Response::TerminalOutput(output)) => IoEvent::Output(output),
                        Ok(_) => IoEvent::Failed("Unexpected terminal response".into()),
                        Err(error) => IoEvent::Failed(message(error)),
                    };
                    let last = !matches!(&event, IoEvent::Output(output) if !output.exited);
                    if events.send(event).await.is_err() || last {
                        return;
                    }
                }
            }));
        }
        // Input, written in order.
        let pending = Arc::new(AtomicUsize::new(0));
        let (writes, mut queue) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        {
            let (client, owner, id) = (client.clone(), owner.clone(), id.clone());
            let (mut events, pending) = (events.clone(), pending.clone());
            tasks.push(runtime.spawn(async move {
                while let Some(data) = queue.recv().await {
                    let length = data.len();
                    let written = client
                        .request(Request::TerminalWrite {
                            owner: owner.clone(),
                            id: id.clone(),
                            data,
                        })
                        .await;
                    pending.fetch_sub(length, Ordering::SeqCst);
                    if let Err(error) = written {
                        let _ = events.send(IoEvent::Failed(message(error))).await;
                        return;
                    }
                }
            }));
        }
        // Resizes, coalesced to the latest size.
        let (size, mut sizes) = tokio::sync::watch::channel(START_SIZE);
        {
            let (client, owner, id) = (client.clone(), owner.clone(), id.clone());
            let mut events = events.clone();
            tasks.push(runtime.spawn(async move {
                while sizes.changed().await.is_ok() {
                    let (cols, rows) = *sizes.borrow_and_update();
                    let resized = client
                        .request(Request::TerminalResize {
                            owner: owner.clone(),
                            id: id.clone(),
                            cols,
                            rows,
                        })
                        .await;
                    if let Err(error) = resized {
                        let _ = events.send(IoEvent::Failed(message(error))).await;
                        return;
                    }
                }
            }));
        }
        drop(events);
        let events_key = key.clone();
        let watcher = cx.spawn(async move |this, cx| {
            while let Some(event) = received.next().await {
                let more = this
                    .update(cx, |this, cx| this.on_event(&events_key, serial, event, cx))
                    .unwrap_or(false);
                if !more {
                    return;
                }
            }
        });
        let screen = cx.new(|_| Screen::new(START_SIZE.0 as usize, START_SIZE.1 as usize));
        let Some(session) = self.sessions.get_mut(&key) else {
            return;
        };
        session.owner = Some(owner);
        session.id = Some(id);
        session.screen = Some(screen);
        session.io = Some(Io {
            writes,
            pending,
            size,
            tasks,
            _events: watcher,
        });
        session.status = Status::Running;
        cx.notify();
    }

    /// Applies output or a failure; false once the session stops reading.
    fn on_event(&mut self, key: &Key, serial: u64, event: IoEvent, cx: &mut Context<Self>) -> bool {
        if !self.live(key, serial) {
            return false;
        }
        match event {
            IoEvent::Failed(error) => {
                if let Some(session) = self.sessions.get(key)
                    && let Some(owner) = session.owner.clone()
                    && error == OFFLINE
                {
                    let host = session.host;
                    self.forget_owner(host, &owner);
                }
                self.fail(key, serial, error, cx);
                false
            }
            IoEvent::Output(output) => {
                let theme = Theme::from_colors(cx.colors());
                let cell = self.cell;
                let Some(session) = self.sessions.get(key) else {
                    return false;
                };
                if let Some(screen) = session.screen.clone()
                    && !output.data.is_empty()
                {
                    let effects = screen.update(cx, |screen, cx| {
                        let effects = screen.feed(&output.data, &theme, cell);
                        cx.notify();
                        effects
                    });
                    if let Some(text) = effects.clipboard {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    for reply in effects.replies {
                        self.write(key, reply.into_bytes(), Source::Reply, cx);
                    }
                }
                if output.exited {
                    self.exited(key.clone(), serial, cx);
                    return false;
                }
                true
            }
        }
    }

    /// The shell exited on its own: close it on the host, keep its screen.
    fn exited(&mut self, key: Key, serial: u64, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get_mut(&key) else {
            return;
        };
        session.io = None;
        let (owner, id) = (session.owner.clone(), session.id.clone());
        let Some(host) = Self::hosts(cx)
            .into_iter()
            .find(|host| host.entity_id() == session.host)
        else {
            return;
        };
        let client = host.read(cx).client();
        cx.spawn(async move |this, cx| {
            let closed = match (owner, id) {
                (Some(owner), Some(id)) => {
                    request(&client, Request::TerminalClose { owner, id }, cx).await
                }
                _ => Ok(Response::Done),
            };
            let _ = this.update(cx, |this, cx| {
                if !this.live(&key, serial) {
                    return;
                }
                match closed {
                    Ok(_) => {
                        if let Some(session) = this.sessions.get_mut(&key) {
                            session.id = None;
                            session.status = Status::Exited;
                        }
                        cx.notify();
                    }
                    Err(error) => this.fail(&key, serial, message(error), cx),
                }
            });
        })
        .detach();
    }

    fn fail(&mut self, key: &Key, serial: u64, error: String, cx: &mut Context<Self>) {
        if !self.live(key, serial) {
            return;
        }
        if let Some(session) = self.sessions.get_mut(key) {
            session.error = Some(error);
            session.status = Status::Error;
            session.io = None;
        }
        cx.notify();
    }

    /// A start that can no longer run ends quietly.
    fn retire(&mut self, key: &Key, serial: u64, cx: &mut Context<Self>) {
        if !self.live(key, serial) {
            return;
        }
        if let Some(session) = self.sessions.get_mut(key) {
            session.status = Status::Ended;
            session.io = None;
        }
        cx.notify();
    }

    /// Sends input to the channel's shell. The person's input only goes to
    /// the community on screen; the emulator's replies always go.
    pub fn write(&mut self, key: &Key, data: Vec<u8>, source: Source, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(key) else {
            return;
        };
        if session.status != Status::Running
            || (source == Source::User && !Self::allowed(&session.context, cx))
        {
            return;
        }
        let serial = session.serial;
        let Some(io) = &session.io else {
            return;
        };
        let length = data.len();
        if !fits(io.pending.load(Ordering::SeqCst), length) {
            self.fail(key, serial, INPUT_FULL.into(), cx);
            return;
        }
        io.pending.fetch_add(length, Ordering::SeqCst);
        if io.writes.send(data).is_err() {
            io.pending.fetch_sub(length, Ordering::SeqCst);
        }
    }

    /// Sends the panel's size to the shell, within the PTY's limits.
    pub fn resize(&mut self, key: &Key, cols: usize, rows: usize) {
        let Some(io) = self
            .sessions
            .get(key)
            .filter(|session| session.status == Status::Running)
            .and_then(|session| session.io.as_ref())
        else {
            return;
        };
        let size = (cols.clamp(2, 500) as u16, rows.clamp(1, 300) as u16);
        io.size.send_if_modified(|current| {
            let changed = *current != size;
            *current = size;
            changed
        });
    }

    /// Ends the channel's shell (`end`). On failure the session shows the
    /// error and End can be tried again.
    pub fn end(&mut self, key: &Key, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let Some(session) = self.sessions.get_mut(key) else {
            return Task::ready(Ok(()));
        };
        session.status = Status::Ended;
        session.screen = None;
        session.io = None;
        let serial = session.serial;
        let (owner, id, host) = (session.owner.clone(), session.id.clone(), session.host);
        cx.notify();
        let client = Self::hosts(cx)
            .into_iter()
            .find(|candidate| candidate.entity_id() == host)
            .map(|host| host.read(cx).client());
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let closed = match (client, owner, id) {
                (Some(client), Some(owner), Some(id)) => {
                    request(&client, Request::TerminalClose { owner, id }, cx)
                        .await
                        .map(drop)
                        .map_err(message)
                }
                _ => Ok(()),
            };
            this.update(cx, |this, cx| {
                let current = this
                    .sessions
                    .get(&key)
                    .is_some_and(|session| session.serial == serial);
                match &closed {
                    Ok(()) if current => {
                        this.sessions.remove(&key);
                    }
                    Err(error) if current => {
                        if let Some(session) = this.sessions.get_mut(&key) {
                            session.status = Status::Error;
                            session.error = Some(error.clone());
                        }
                    }
                    _ => {}
                }
                cx.notify();
            })
            .map_err(|_| "Terminal sessions are unavailable".to_string())?;
            closed
        })
    }
}

/// Whether another session may open beside `open` others.
fn admit(open: usize) -> Result<(), String> {
    if open >= MAX_SESSIONS {
        Err(TOO_MANY.into())
    } else {
        Ok(())
    }
}

/// Whether `length` more bytes fit behind `pending` queued ones.
fn fits(pending: usize, length: usize) -> bool {
    pending + length <= MAX_PENDING_INPUT
}

/// Runs a host request on the I/O runtime.
async fn request(
    client: &Arc<HostClient<HostProtocol>>,
    body: Request,
    cx: &AsyncApp,
) -> Result<Response, RpcError> {
    let client = client.clone();
    let runtime = cx.update(|cx| Runtime::handle(cx));
    runtime
        .spawn(async move { client.request(body).await })
        .await
        .unwrap_or(Err(RpcError::Disconnected))
}

/// Closes a shell nobody needs any more.
fn close_later(
    client: Arc<HostClient<HostProtocol>>,
    owner: String,
    id: String,
    cx: &mut Context<Sessions>,
) {
    Runtime::handle(cx).spawn(async move {
        let _ = client.request(Request::TerminalClose { owner, id }).await;
    });
}

/// How a host failure reads in the panel.
fn message(error: RpcError) -> String {
    match error {
        error if error.is_unsupported() => UNSUPPORTED.into(),
        RpcError::NotConnected | RpcError::Disconnected => OFFLINE.into(),
        error => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn twenty_sessions_at_most() {
        assert_eq!(admit(19), Ok(()));
        assert_eq!(
            admit(20),
            Err("Error: Twenty terminal sessions are open. End one before opening another.".into())
        );
    }

    #[test]
    fn input_queues_up_to_one_mebibyte() {
        assert!(fits(0, MAX_PENDING_INPUT));
        assert!(!fits(1, MAX_PENDING_INPUT));
        assert_eq!(
            INPUT_FULL,
            "Terminal input is full. End this session and start again."
        );
    }

    #[test]
    fn statuses_read_as_buzz_app_shows_them() {
        let labels: Vec<_> = [
            Status::Starting,
            Status::Running,
            Status::Exited,
            Status::Error,
            Status::Ended,
        ]
        .map(Status::label)
        .into();
        assert_eq!(labels, ["starting", "running", "exited", "error", "ended"]);
    }

    #[test]
    fn an_older_host_is_named_as_such() {
        let old =
            RpcError::Failed("The other side does not support this request; update it.".into());
        assert_eq!(message(old), UNSUPPORTED);
        assert_eq!(
            message(RpcError::Failed("Invalid terminal channel UUID".into())),
            "Invalid terminal channel UUID"
        );
    }
}
