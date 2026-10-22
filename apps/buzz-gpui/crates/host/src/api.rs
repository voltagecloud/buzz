//! The host protocol: what a GUI can ask an agent host, and what the host
//! pushes back.
//!
//! These types are ours, not the controller's, so a host and a GUI built
//! from different controller revisions still understand each other; fields
//! one side does not know default.
//!
//! The v1 agent requests are gone with the v1 agents. A host answers an
//! older GUI's v1 request as one it does not support, as it answers any
//! request it cannot read, and a GUI skips an older host's v1 snapshot
//! events as it skips any event it cannot read.

use buzz_host_rpc::Protocol;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod app_agent;
pub use app_agent::*;
mod sandbox;
pub use sandbox::*;

pub use crate::terminal::{TerminalContext, TerminalOutput};

pub struct HostProtocol;

impl Protocol for HostProtocol {
    type ClientRequest = Request;
    type ClientResponse = Response;
    type HostRequest = HostAsk;
    type HostResponse = HostAnswer;
    type Event = HostEvent;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    // Agents2 custody
    /// An Agents2 agent's key, held by this host: see [`AppAgentRequest`].
    AppAgent(AppAgentRequest),
    /// An agent's remote workspace: see [`SandboxRequest`]. Hosts from
    /// before this answer that they do not support it.
    Sandbox(SandboxRequest),
    // claude code
    /// Starts process `process`, one that agent type `owner` declares (see
    /// `processes.rs`), with `args` after its declared arguments. `handle`
    /// is chosen by the GUI, unique on its connection, so output can arrive
    /// before this request's response. Output and the exit code come back
    /// as [`HostAsk::ProcessOutput`] on this connection, which owns the
    /// process: it is killed when the connection closes.
    ProcessSpawn {
        handle: u64,
        owner: String,
        process: String,
        args: Vec<String>,
        /// Absolute, or under home written `~/…`; created if missing.
        #[serde(default)]
        cwd: Option<String>,
        /// Added to the host's environment; `None` removes a variable.
        #[serde(default)]
        env: BTreeMap<String, Option<String>>,
    },
    /// Writes `data` to the process's stdin; `close` then ends its input.
    ProcessWrite {
        handle: u64,
        data: String,
        close: bool,
    },
    /// Asks the process to stop, then kills it and its descendants. Its
    /// exit follows as output.
    ProcessKill {
        handle: u64,
    },
    /// Registers a terminal owner. Its sessions end when it closes or the
    /// connection that made it does. Answered with
    /// [`Response::TerminalOwner`].
    TerminalOwner,
    /// Starts the host user's login shell. Answered with
    /// [`Response::TerminalSession`].
    TerminalSpawn {
        owner: String,
        context: TerminalContext,
        cols: u16,
        rows: u16,
    },
    /// The session's output since the last read, waiting up to `wait_ms`
    /// for some to arrive. Answered with [`Response::TerminalOutput`].
    TerminalRead {
        owner: String,
        id: String,
        #[serde(default)]
        wait_ms: u32,
    },
    TerminalWrite {
        owner: String,
        id: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    TerminalResize {
        owner: String,
        id: String,
        cols: u16,
        rows: u16,
    },
    TerminalClose {
        owner: String,
        id: String,
    },
    TerminalCloseOwner {
        owner: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    /// Answers requests that return nothing.
    Done,
    // Agents2 custody
    AppAgent(AppAgentResponse),
    Sandbox(SandboxResponse),
    // claude code
    /// A process request succeeded.
    ProcessDone,
    TerminalOwner(String),
    TerminalSession(String),
    TerminalOutput(TerminalOutput),
}

/// Requests the host makes of the GUI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostAsk {
    // claude code
    /// Output of a process this connection spawned, in order. An
    /// [`ProcessOutput::Exit`] is always the last.
    ProcessOutput {
        handle: u64,
        output: Vec<ProcessOutput>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostAnswer {
    // claude code
    /// The output was taken. An error instead stops the process.
    ProcessReceived,
}

/// State changes the host pushes to every connected GUI; none since the
/// v1 agent snapshots went.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostEvent {}

// claude code
/// What a spawned process printed, or how it ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessOutput {
    Stdout(String),
    Stderr(String),
    /// `None` when a signal ended it.
    Exit(Option<i32>),
}
