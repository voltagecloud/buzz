//! Thread views (`threads.ts`, `thread-window.ts`;
//! spec §5c).
//!
//! Everything here is pure and synchronous: [`ThreadState`] never performs
//! I/O. It hands out the next read as a [`ThreadRead`] and takes the result
//! back through [`ThreadState::complete`], so the session actor stays the
//! only owner of the reader, the access epoch and the outbox.
//!
//! # Driving a thread view from the session
//!
//! One [`ThreadState`] per open thread panel, keyed by the session (the
//! reference caps owned views at 64 across threads and filtered views).
//!
//! 1. **Open.** Build a [`ThreadConfig`] with the relay's NIP-11 `self` as
//!    both `relay_author` and `signing_authority`, the viewer's key, and
//!    `authority_host = thread_authority(origin.websocket()).ok()`. Pass the
//!    session's shared [`MessageClock`] and, as `seed`, the message if the
//!    session already holds it (a retained window or another thread's
//!    [`ThreadState::event`]), but only when the channel is readable. Then
//!    call [`ThreadState::receive`] with every recently observed event and
//!    [`ThreadState::set_local`] with the outbox's signed operations (the
//!    same list `local_changed` gives timelines).
//! 2. **Reads.** [`ThreadState::refresh`] and [`ThreadState::load_more`]
//!    return the first read of a run, or `None` when nothing should start.
//!    Run `read.filters` through `Reader::read` with `read.options`
//!    (foreground, not fresh), in a task that reports back to the actor.
//!    Before reporting, if the channel is no longer readable, report a
//!    `ReadError` (`Unavailable`) instead. Back on the actor, call
//!    [`ThreadState::complete`] with the read's `token` and the result.
//!    Thread windows are verified inside `complete`, before anything is
//!    admitted, so the session must **not** pass a strict page's events to
//!    other stores itself.
//! 3. **Completion.** [`Completion::admit`] lists verified events the
//!    session should now run through its `accept()` pipeline (profiles,
//!    outbox echo, timelines and every thread's `receive`). It is empty for
//!    a page whose bounds failed, and an exact target is held back, with
//!    its overlays, until it folded completely. When
//!    [`Completion::admission`] is set, the list is that complete fold:
//!    hand the events `accept()` kept to [`ThreadState::admitted`], which
//!    goes on from there (`acceptTarget`). [`Completion::denied`] means
//!    the relay refused the read (401/403): deny the channel in discovery
//!    and purge as for any revocation; this view already revoked itself.
//!    [`Completion::next`] is the next read of the same run; start it like
//!    the first.
//! 4. **Live and local.** Call [`ThreadState::receive`] for every verified
//!    batch `accept()` sees, and [`ThreadState::set_local`] whenever the
//!    outbox changes. Neither starts a read.
//! 5. **Lifecycle.** On revocation of the channel call
//!    [`ThreadState::revoke`] (and [`ThreadState::regrant`] if access
//!    returns while the view is open); on a cache clear,
//!    [`ThreadState::clear`]; when the live route for the channel is
//!    re-established, [`ThreadState::refresh`] to repair the retained range.
//!    Each of these fences reads in flight: a stale token's result is
//!    ignored by `complete`, so the session needs no extra epoch check.
//!    Dropping the state disposes the view.
//! 6. **Output.** [`ThreadState::take_snapshot`] returns the new
//!    [`ThreadSnapshot`] once after anything visible changed; send it on
//!    the view's watch channel. Unchanged rows keep their `Arc`.
//! 7. **Retarget.** [`ThreadState::retarget`] selects another message of
//!    the open thread without reloading it. Run its reads like any other;
//!    [`Completion::retarget`] carries the outcome once it ends. When
//!    [`ThreadState::selecting`] turns false without one, it was cancelled.
//!
//! The reply tree is built here ([`ThreadSnapshot::tree`], from
//! [`reply_tree`]) and rebuilt only when the replies change, so the UI can
//! render nesting without walking ancestry itself.
//!
//! Thread follows live with unread (`Unread::follow`, saved by
//! `SqliteThreadFollows`), as in the reference's `unread.ts`.

mod window;

#[cfg(test)]
mod tests;

pub use window::{
    CONTENT_KINDS, DEPTH_LIMIT, EVIDENCE_LIMIT, INITIAL_WINDOW_SIZE, MAX_CURSOR_TIME, PAGE_SIZE,
    STRICT_WINDOW_KINDS, TARGET_OVERLAY_KINDS, TOMBSTONE_KINDS, ThreadBounds, ThreadCursor,
    ThreadWindowError, WINDOW_FIELDS, is_canonical_channel, legacy_filter, message_filter,
    overlays_filter, thread_authority, thread_binding, thread_bounds, tombstones_filter,
    verify_thread_windows, window_filter,
};

use crate::event::{Verified, kinds};
use crate::fold::{
    Delivery, FoldOptions, LocalDelivery, MessageClock, MessageProjection, MessageRow, ReplyTree,
    e_targets, reply_tree, thread_reference,
};
use crate::reader::{Events, ReadError, ReadErrorKind, ReadOptions};
use crate::socket::Filter;
use nostr::prelude::{EventId, PublicKey};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;

/// Pages one view may hold: 10 + 9 × 50 = 460 strict replies at most.
pub const MAX_PAGES: usize = 10;
/// Retained evidence cap. Over it the view clears rather than evicting a
/// deletion and resurrecting what it deleted.
pub const MAX_EVENTS: usize = 2000;
pub const MAX_BYTES: usize = 4 << 20;

/// Events that only describe others (`threads.ts:18`).
const AUX: [u16; 6] = [
    kinds::DELETION,
    kinds::REACTION,
    kinds::MODERATOR_DELETE,
    kinds::EDIT,
    kinds::THREAD_SUMMARY,
    kinds::CHANNEL_WINDOW,
];
/// Private product feedback never enters a view (`session.ts`).
const PRODUCT_FEEDBACK: u16 = 42000;

const MEMORY_LIMIT: &str =
    "Thread view exceeded its memory limit. Refresh to read from the beginning.";
const ROOT_UNAVAILABLE: &str = "The original thread message is unavailable.";
const SELECTED_UNAVAILABLE: &str = "The selected message is unavailable.";
const NOT_ADVANCING: &str = "Thread pagination did not advance. Refresh to try again.";
const EVIDENCE_EXCEEDED: &str = "Selected message exceeded its evidence limit";
const INTERRUPTED: &str = "Thread read interrupted. Refresh to continue.";
const NO_ACCESS: &str = "This channel is no longer available.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadStatus {
    Idle,
    Loading,
    Ready,
    Error,
}

/// How the loaded range grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Strict signed windows, newest first.
    Older,
    /// The legacy bounded walk, oldest first.
    Forward,
}

/// The read in flight or that last failed, apart from the direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadKind {
    Older,
    Refresh,
}

/// The exact-navigation target's state (§5c.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetStatus {
    Loading,
    Ready,
    /// Missing, deleted or not readable.
    Unavailable,
    Error,
}

/// What a thread panel renders.
#[derive(Clone, Debug)]
pub struct ThreadSnapshot {
    pub status: ThreadStatus,
    /// `None` until loaded, and when the original message is unavailable.
    pub root: Option<Arc<MessageRow>>,
    /// Every other row in rendered order.
    pub replies: Arc<[Arc<MessageRow>]>,
    /// The replies nested by signed ancestry under the root.
    pub tree: Arc<ReplyTree>,
    pub error: Option<String>,
    /// Continuation is possible; not a claim about the thread's size.
    pub can_load_more: bool,
    /// More exists but the view holds its maximum.
    pub limited: bool,
    pub direction: Option<Direction>,
    pub read_kind: Option<ReadKind>,
    /// Exact mode only: the selected message, folded on its own evidence.
    pub target: Option<Arc<MessageRow>>,
    /// Exact mode only.
    pub target_status: Option<TargetStatus>,
}

impl ThreadSnapshot {
    /// Before anything loaded.
    pub fn idle(exact: bool) -> Self {
        Self {
            status: ThreadStatus::Idle,
            root: None,
            replies: Arc::new([]),
            tree: Arc::default(),
            error: None,
            can_load_more: false,
            limited: false,
            direction: None,
            read_kind: None,
            target: None,
            target_status: exact.then_some(TargetStatus::Loading),
        }
    }

    fn same(&self, other: &Self) -> bool {
        let same_row = |a: &Option<Arc<MessageRow>>, b: &Option<Arc<MessageRow>>| match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        self.status == other.status
            && same_row(&self.root, &other.root)
            && Arc::ptr_eq(&self.replies, &other.replies)
            && Arc::ptr_eq(&self.tree, &other.tree)
            && self.error == other.error
            && self.can_load_more == other.can_load_more
            && self.limited == other.limited
            && self.direction == other.direction
            && self.read_kind == other.read_kind
            && same_row(&self.target, &other.target)
            && self.target_status == other.target_status
    }
}

#[derive(Clone, Debug)]
pub struct ThreadConfig {
    pub channel: String,
    /// The message the view was opened on: the root, or a reply in exact
    /// mode.
    pub message_id: EventId,
    /// Fold the selected message on its own evidence first (deep links
    /// and reveal of a reply, §5c.5).
    pub exact: bool,
    /// NIP-11 `self`: the only signer of 39005 and 39007.
    pub relay_author: PublicKey,
    /// The key whose messages are attributed through `actor`/`p` tags.
    pub signing_authority: Option<PublicKey>,
    pub viewer: PublicKey,
    /// [`thread_authority`] of the relay origin. `None` makes every strict
    /// page invalid instead of falling back.
    pub authority_host: Option<String>,
}

/// Which read of a run this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadStep {
    /// Exact mode: the selected message by id.
    Target,
    /// Exact mode: overlays of the selected message.
    Overlays,
    /// Exact mode: deletions of those overlays.
    Tombstones,
    /// The opened message by id, to find its root.
    Selected,
    /// The root by id, once per strict run.
    Root,
    /// A strict `thread_window` page.
    Window,
    /// A legacy page, batched with the root lookup.
    Legacy,
}

/// A read for the session to run.
#[derive(Clone, Debug)]
pub struct ThreadRead {
    pub token: u64,
    pub step: ReadStep,
    pub filters: Vec<Filter>,
    pub options: ReadOptions,
}

/// What the session does after [`ThreadState::complete`].
#[derive(Debug, Default)]
pub struct Completion {
    /// Verified events to run through the session's `accept()` pipeline.
    pub admit: Vec<Verified>,
    /// `admit` is an exact target's complete fold: pass the events
    /// `accept()` kept to [`ThreadState::admitted`].
    pub admission: bool,
    /// The next read of this run.
    pub next: Option<ThreadRead>,
    /// The relay refused the read: deny the channel.
    pub denied: bool,
    /// A [`ThreadState::retarget`] ended with this.
    pub retarget: Option<Retarget>,
}

/// What selecting another message in an open thread came to
/// (`ThreadRetarget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retarget {
    /// The message is now the view's selected target.
    Selected,
    /// Its signed ancestry names another thread: open that one instead.
    Different,
    /// Missing, deleted or not readable.
    Unavailable,
    Error,
    /// A newer selection, access loss, a cache clear or closing the view
    /// ended it.
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Strict, until the first page shows whether the relay signs windows.
    Probe,
    /// Strict, proven.
    Older,
    Legacy,
}

/// Fields a publish carries over unless changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fields {
    status: ThreadStatus,
    error: Option<String>,
    can_load_more: bool,
    limited: bool,
    direction: Option<Direction>,
    read_kind: Option<ReadKind>,
}

struct Pending {
    token: u64,
    step: ReadStep,
    filters: Vec<Filter>,
}

/// One `run(replace)` of the reference, suspended between reads.
struct Run {
    /// The selected message this run's exact reads fold. A retarget
    /// during the run leaves its status to the retarget.
    target: EventId,
    target_pages: usize,
    next_cursor: Option<ThreadCursor>,
    next_pages: usize,
    fetched: HashMap<EventId, Verified>,
    /// The verified root of this run, prepended to every strict page.
    root: Vec<Verified>,
    page: usize,
    /// The exact target read, staged until its fold is admitted.
    selected: Option<Verified>,
    pending: Option<Pending>,
}

/// A [`ThreadState::retarget`] between reads. It runs beside any run, so
/// paging and repair neither interrupt it nor are interrupted by it.
struct Selection {
    id: EventId,
    token: u64,
    step: ReadStep,
    /// The candidate and its overlays, read but not yet safe to show.
    staged: EventMap,
}

type EventMap = HashMap<EventId, Verified>;

/// Whose target fold the session is admitting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admitting {
    /// The run's exact target.
    Target,
    /// The retarget's candidate.
    Selection,
}

/// One thread view (`createThreadView`). See the module docs for how the
/// session drives it.
pub struct ThreadState {
    config: ThreadConfig,
    viewer_hex: String,
    relay_hex: String,
    projection: MessageProjection,
    mode: Mode,
    root_id: Option<EventId>,
    root_unavailable: bool,
    target_status: Option<TargetStatus>,
    /// Retained fetched and live evidence. Bounded reads never retract it.
    remote: EventMap,
    /// Exact overlays read but not yet safe to show.
    staged: EventMap,
    local: Vec<(Verified, LocalDelivery)>,
    cursor: Option<ThreadCursor>,
    pages: usize,
    run: Option<Run>,
    selection: Option<Selection>,
    /// A complete target fold waits for the session's admission.
    admitting: Option<Admitting>,
    /// A refresh was asked for while a run was in flight.
    again: bool,
    accessible: bool,
    next_token: u64,
    fields: Fields,
    tree_root: Option<EventId>,
    current: Arc<ThreadSnapshot>,
    changed: bool,
}

impl ThreadState {
    pub fn new(config: ThreadConfig, clock: Arc<MessageClock>, seed: Option<&Verified>) -> Self {
        let projection = MessageProjection::new(
            config.channel.clone(),
            config.relay_author,
            FoldOptions {
                include_replies: true,
                signing_authority: config.signing_authority,
                ..Default::default()
            },
        )
        .with_clock(clock);
        let mode = if is_canonical_channel(&config.channel) {
            Mode::Probe
        } else {
            Mode::Legacy
        };
        let exact = config.exact;
        let mut state = Self {
            viewer_hex: config.viewer.to_hex(),
            relay_hex: config.relay_author.to_hex(),
            config,
            projection,
            mode,
            root_id: None,
            root_unavailable: false,
            target_status: exact.then_some(TargetStatus::Loading),
            remote: HashMap::new(),
            staged: HashMap::new(),
            local: Vec::new(),
            cursor: None,
            pages: 0,
            run: None,
            selection: None,
            admitting: None,
            again: false,
            accessible: true,
            next_token: 1,
            fields: Fields {
                status: ThreadStatus::Idle,
                error: None,
                can_load_more: false,
                limited: false,
                direction: None,
                read_kind: None,
            },
            tree_root: None,
            current: Arc::new(ThreadSnapshot::idle(exact)),
            changed: false,
        };
        if let Some(seed) = seed
            && is_content(seed)
            && state.in_channel(seed)
        {
            let root = thread_reference(seed).map_or(seed.id, |reference| reference.root_id);
            state.root_id = Some(root);
            if seed.id == root {
                state.remote.insert(seed.id, seed.clone());
            }
            state.publish();
        }
        state
    }

    pub fn channel(&self) -> &str {
        &self.config.channel
    }

    pub fn message_id(&self) -> &EventId {
        &self.config.message_id
    }

    pub fn exact(&self) -> bool {
        self.config.exact
    }

    /// The thread root, once known.
    pub fn root_id(&self) -> Option<&EventId> {
        self.root_id.as_ref()
    }

    /// A read of this view is waiting for its result.
    pub fn loading(&self) -> bool {
        self.run.is_some()
    }

    pub fn snapshot(&self) -> Arc<ThreadSnapshot> {
        self.current.clone()
    }

    /// The snapshot, once after each visible change.
    pub fn take_snapshot(&mut self) -> Option<Arc<ThreadSnapshot>> {
        std::mem::take(&mut self.changed).then(|| self.current.clone())
    }

    /// A retained or staged event, so the session can check a live overlay
    /// or seed another view without a read.
    pub fn event(&self, id: &EventId) -> Option<&Verified> {
        self.remote
            .get(id)
            .or_else(|| self.staged.get(id))
            .or_else(|| self.selection.as_ref()?.staged.get(id))
    }

    /// A [`retarget`](Self::retarget) is waiting for a read.
    pub fn selecting(&self) -> bool {
        self.selection.is_some()
    }

    /// Selects `id` in this thread without reloading it (`retarget`). The
    /// rows, pages and cursor stay; the selection changes only once `id`
    /// is verified, by its own signed ancestry, to belong to this thread
    /// and its overlays have folded. Returns the first read, or the
    /// outcome when no read is needed. A later call supersedes this one;
    /// revocation, a cache clear or an over-budget fold cancels it.
    pub fn retarget(&mut self, id: EventId) -> Result<ThreadRead, Retarget> {
        if !self.accessible {
            return Err(Retarget::Cancelled);
        }
        // A route hint is never evidence that two messages share a thread.
        if self.root_id.is_none() || self.current.root.is_none() {
            return Err(Retarget::Different);
        }
        self.selection = Some(Selection {
            id,
            token: 0,
            step: ReadStep::Target,
            staged: HashMap::new(),
        });
        let filter = message_filter(&self.config.channel, &id);
        match self.issue_selection(ReadStep::Target, vec![filter]) {
            ControlFlow::Continue(Some(read)) => Ok(read),
            ControlFlow::Continue(None) => Err(Retarget::Cancelled),
            ControlFlow::Break(outcome) => Err(outcome),
        }
    }

    /// Reloads the whole retained range from the newest page.
    pub fn refresh(&mut self) -> Option<ThreadRead> {
        self.start(true)
    }

    /// Loads the next page, if the last run left one to load.
    pub fn load_more(&mut self) -> Option<ThreadRead> {
        self.start(false)
    }

    /// Feeds back the result of a read this view handed out. Results of
    /// fenced reads are ignored.
    pub fn complete(&mut self, token: u64, result: Result<Events, ReadError>) -> Completion {
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.token == token)
        {
            return self.complete_selection(result);
        }
        let mut out = Completion::default();
        let Some(run) = self.run.as_mut() else {
            return out;
        };
        if run
            .pending
            .as_ref()
            .is_none_or(|pending| pending.token != token)
        {
            return out;
        }
        let Pending { step, filters, .. } = run.pending.take().expect("pending read");
        let events = match result {
            Err(error) if error.kind == ReadErrorKind::Denied => {
                out.denied = true;
                self.revoke();
                return out;
            }
            Err(error) => {
                out.next = self.fail(error.message);
                return out;
            }
            Ok(events) => events,
        };
        // The exact reader refuses a capped raw response: a fold over a
        // truncated overlay set is not safe (`session.ts:1822-1829`).
        if self.config.exact
            && events.len() >= EVIDENCE_LIMIT
            && !filters
                .iter()
                .any(|filter| filter.contains_key("depth_limit"))
        {
            out.next = self.fail(EVIDENCE_EXCEEDED.into());
            return out;
        }
        let events: Vec<Verified> = events
            .iter()
            .filter(|event| event.kind_u16() != PRODUCT_FEEDBACK)
            .cloned()
            .collect();
        out.next = match step {
            ReadStep::Target => self.on_target(events),
            ReadStep::Overlays => self.on_overlays(events, &mut out.admit),
            ReadStep::Tombstones => self.on_tombstones(events, &mut out.admit),
            ReadStep::Selected => {
                out.admit.clone_from(&events);
                self.on_selected(events)
            }
            ReadStep::Root => {
                out.admit.clone_from(&events);
                self.on_root(events)
            }
            ReadStep::Window => self.on_window(&filters, events, &mut out.admit),
            ReadStep::Legacy => {
                out.admit.clone_from(&events);
                self.on_page(events, None)
            }
        };
        out.admission = self.admitting.is_some();
        out
    }

    /// The session admitted a target fold this view handed out with
    /// [`Completion::admission`]; `visible` is what its `accept()` kept.
    /// A target it refused is unavailable, and only an admitted one names
    /// the thread's root (`readTarget`).
    pub fn admitted(&mut self, visible: &[Verified]) -> Completion {
        let mut out = Completion::default();
        match self.admitting.take() {
            Some(Admitting::Target) => out.next = self.target_admitted(visible),
            Some(Admitting::Selection) => match self.selection_admitted(visible) {
                ControlFlow::Continue(read) => out.next = Some(read),
                ControlFlow::Break(outcome) => {
                    self.selection = None;
                    out.retarget = Some(outcome);
                }
            },
            None => {}
        }
        out
    }

    /// Takes verified live or read evidence that belongs to this thread.
    /// Returns whether this call changed the snapshot.
    pub fn receive(&mut self, events: &[Verified]) -> bool {
        if !self.accessible || (self.root_id.is_none() && !self.config.exact) {
            return false;
        }
        let incoming: Vec<Verified> = self
            .related(events)
            .into_iter()
            .filter(|event| !self.remote.contains_key(&event.id))
            .collect();
        if incoming.is_empty() {
            return false;
        }
        let before = self.current.clone();
        if self.extend_remote(incoming) {
            self.publish();
        }
        !Arc::ptr_eq(&before, &self.current)
    }

    /// The channel's admins from its 39001, whose moderator deletes apply.
    /// Returns whether this call changed the snapshot.
    pub fn set_admins(&mut self, admins: &HashSet<PublicKey>) -> bool {
        if self.projection.options().channel_admins == *admins {
            return false;
        }
        let options = FoldOptions {
            channel_admins: admins.clone(),
            ..self.projection.options().clone()
        };
        let (_, refolded) = self.projection.set_options(options);
        if !refolded {
            return false;
        }
        let before = self.current.clone();
        self.publish();
        !Arc::ptr_eq(&before, &self.current)
    }

    /// Replaces the outbox operations this view may show. Failed replies
    /// stay visible for retry; failed edits and reactions do not apply.
    pub fn set_local(&mut self, operations: &[(Verified, LocalDelivery)]) {
        let unchanged = self.local.len() == operations.len()
            && self
                .local
                .iter()
                .zip(operations)
                .all(|((old, was), (new, is))| old.id == new.id && was == is);
        if unchanged {
            return;
        }
        self.local = operations.to_vec();
        self.publish();
    }

    /// Stops the run in flight and keeps what was loaded.
    pub fn interrupt(&mut self) {
        self.purge(false);
    }

    /// Stops the run in flight and forgets everything (cache clear).
    pub fn clear(&mut self) {
        self.purge(true);
    }

    /// The channel became unreadable: stop, forget, and refuse new runs
    /// until [`regrant`](Self::regrant).
    pub fn revoke(&mut self) {
        self.accessible = false;
        self.purge(false);
    }

    /// The channel is readable again. Nothing is reloaded until a refresh.
    pub fn regrant(&mut self) {
        self.accessible = true;
    }

    // ----- runs -----

    fn start(&mut self, replace: bool) -> Option<ThreadRead> {
        if self.run.is_some() {
            if replace {
                self.again = true;
            }
            return None;
        }
        if !self.accessible {
            self.purge(false);
            return None;
        }
        if !replace && (!self.fields.can_load_more || self.fields.limited) {
            return None;
        }
        self.run = Some(Run {
            target: self.config.message_id,
            target_pages: if replace { self.pages.max(1) } else { 1 },
            next_cursor: if replace { None } else { self.cursor.clone() },
            next_pages: if replace { 0 } else { self.pages },
            fetched: HashMap::new(),
            root: Vec::new(),
            page: 0,
            selected: None,
            pending: None,
        });
        // A repair keeps an already-shown target; only a new or missing one
        // waits for its fold.
        if self.config.exact && replace && self.target_status != Some(TargetStatus::Ready) {
            self.target_status = Some(TargetStatus::Loading);
        }
        self.fields.status = ThreadStatus::Loading;
        self.fields.error = None;
        self.fields.read_kind = Some(if replace {
            ReadKind::Refresh
        } else {
            ReadKind::Older
        });
        self.publish_status();
        let selected = message_filter(&self.config.channel, &self.config.message_id);
        if self.config.exact && replace {
            self.issue(ReadStep::Target, vec![selected])
        } else if self.root_id.is_none() {
            self.issue(ReadStep::Selected, vec![selected])
        } else {
            self.begin_root()
        }
    }

    fn issue(&mut self, step: ReadStep, filters: Vec<Filter>) -> Option<ThreadRead> {
        let run = self.run.as_mut()?;
        let token = self.next_token;
        self.next_token += 1;
        run.pending = Some(Pending {
            token,
            step,
            filters: filters.clone(),
        });
        Some(ThreadRead {
            token,
            step,
            filters,
            options: ReadOptions::default(),
        })
    }

    fn begin_root(&mut self) -> Option<ThreadRead> {
        let root = self.root_id?;
        if self.mode == Mode::Legacy {
            return self.begin_page();
        }
        // Strict reads cannot batch an id lookup with a window. Revalidate
        // the root once per run, not once per page of a repair.
        let filter = message_filter(&self.config.channel, &root);
        self.issue(ReadStep::Root, vec![filter])
    }

    fn begin_page(&mut self) -> Option<ThreadRead> {
        let root = self.root_id?;
        let run = self.run.as_ref()?;
        let channel = &self.config.channel;
        let cursor = run.next_cursor.as_ref();
        match self.mode {
            Mode::Legacy => {
                let filters = vec![
                    message_filter(channel, &root),
                    legacy_filter(channel, &root, cursor),
                ];
                self.issue(ReadStep::Legacy, filters)
            }
            Mode::Probe | Mode::Older => {
                let limit = if run.next_pages == 0 {
                    INITIAL_WINDOW_SIZE
                } else {
                    PAGE_SIZE
                };
                let filter = window_filter(channel, &root, limit, cursor);
                self.issue(ReadStep::Window, vec![filter])
            }
        }
    }

    fn on_target(&mut self, events: Vec<Verified>) -> Option<ThreadRead> {
        // The candidate is only checked here. Nothing of it reaches the
        // session's reconciliation, or names the root, until its whole
        // fold is admitted (`readTarget`).
        let target = self.run.as_ref()?.target;
        let Some(event) = self.selected_in(&events, &target) else {
            return self.target_unavailable();
        };
        // Staged, a live deletion of it resolves without showing an
        // incomplete fold.
        self.staged.clear();
        self.staged.insert(event.id, event.clone());
        if union_over_budget(&[&self.remote, &self.staged]) {
            self.overflow();
            return None;
        }
        self.run.as_mut()?.selected = Some(event);
        let filter = overlays_filter(&target);
        self.issue(ReadStep::Overlays, vec![filter])
    }

    /// The run's exact target cannot show: missing, refused, or folded
    /// away.
    fn target_unavailable(&mut self) -> Option<ThreadRead> {
        self.staged.clear();
        if self.superseded() {
            // A retarget chose another message meanwhile; the thread
            // itself is still read.
            return self.begin_root();
        }
        self.target_status = Some(TargetStatus::Unavailable);
        self.fields.status = ThreadStatus::Ready;
        self.fields.can_load_more = false;
        self.publish();
        self.end_run()
    }

    /// A retarget changed the selection after this run read its target.
    fn superseded(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|run| run.target != self.config.message_id)
    }

    fn on_overlays(
        &mut self,
        events: Vec<Verified>,
        admit: &mut Vec<Verified>,
    ) -> Option<ThreadRead> {
        let selected = self.run.as_ref()?.target;
        for event in self.related_to(&events, Some(selected)) {
            self.staged.insert(event.id, event);
        }
        if union_over_budget(&[&self.remote, &self.staged]) {
            self.overflow();
            return None;
        }
        let overlays = overlays_of(&[&self.remote, &self.staged], &selected);
        if overlays.is_empty() {
            return self.finish_target(admit);
        }
        self.issue(ReadStep::Tombstones, vec![tombstones_filter(&overlays)])
    }

    fn on_tombstones(
        &mut self,
        events: Vec<Verified>,
        admit: &mut Vec<Verified>,
    ) -> Option<ThreadRead> {
        let selected = self.run.as_ref()?.target;
        for event in self.related_to(&events, Some(selected)) {
            self.staged.insert(event.id, event);
        }
        if union_over_budget(&[&self.remote, &self.staged]) {
            self.overflow();
            return None;
        }
        self.finish_target(admit)
    }

    /// Hands the target with its complete overlay closure to the session
    /// at once, then waits for [`admitted`](Self::admitted).
    fn finish_target(&mut self, admit: &mut Vec<Verified>) -> Option<ThreadRead> {
        admit.extend(fold_evidence(&self.remote, &self.staged));
        self.admitting = Some(Admitting::Target);
        None
    }

    /// The session admitted the target's fold: the target, if it kept
    /// it, now names the root and shows.
    fn target_admitted(&mut self, visible: &[Verified]) -> Option<ThreadRead> {
        let selected = self.run.as_ref()?.selected.clone()?;
        if !visible.iter().any(|event| event.id == selected.id) {
            return self.target_unavailable();
        }
        // Refused evidence never names a root that later live traffic
        // could fill in; only an admitted target does.
        self.root_id = Some(thread_reference(&selected).map_or(selected.id, |r| r.root_id));
        let related = self.related_to(visible, Some(selected.id));
        if !self.extend_remote(related) {
            return None;
        }
        self.staged.clear();
        if self.superseded() {
            return self.begin_root();
        }
        self.target_status = Some(TargetStatus::Ready);
        self.publish();
        if self.current.target_status != Some(TargetStatus::Ready) {
            // Folded away: deleted by evidence already known.
            self.fields.status = ThreadStatus::Ready;
            self.fields.can_load_more = false;
            self.publish();
            return self.end_run();
        }
        self.begin_root()
    }

    fn on_selected(&mut self, events: Vec<Verified>) -> Option<ThreadRead> {
        let Some(event) = self.selected_in(&events, &self.config.message_id) else {
            return self.fail(SELECTED_UNAVAILABLE.into());
        };
        self.root_id = Some(thread_reference(&event).map_or(event.id, |r| r.root_id));
        self.begin_root()
    }

    fn on_root(&mut self, events: Vec<Verified>) -> Option<ThreadRead> {
        if !self.has_root(&events) {
            return self.root_missing();
        }
        let related = self.related(&events);
        if !self.extend_remote(related) {
            return None;
        }
        self.run.as_mut()?.root = events;
        self.begin_page()
    }

    fn on_window(
        &mut self,
        filters: &[Filter],
        events: Vec<Verified>,
        admit: &mut Vec<Verified>,
    ) -> Option<ThreadRead> {
        let verified = verify_thread_windows(
            filters,
            &events,
            self.config.authority_host.as_deref(),
            &self.viewer_hex,
            &self.relay_hex,
        );
        match verified {
            Err(ThreadWindowError::Missing { legacy_rows: true }) if self.mode == Mode::Probe => {
                // A 200 without bounds is an old relay, not support. Its rows
                // never entered the session; restart without a reverse cursor.
                self.mode = Mode::Legacy;
                let run = self.run.as_mut()?;
                run.next_cursor = None;
                run.next_pages = 0;
                self.begin_page()
            }
            Err(error) => self.fail(error.to_string()),
            Ok(Some(bounds)) if bounds.len() == 1 => {
                self.mode = Mode::Older;
                let page: Vec<Verified> = events
                    .into_iter()
                    .filter(|event| event.kind_u16() != kinds::THREAD_WINDOW)
                    .collect();
                admit.extend(page.iter().cloned());
                let mut response = self.run.as_ref()?.root.clone();
                response.extend(page);
                let bounds = bounds.into_iter().next();
                self.on_page(response, bounds)
            }
            Ok(_) => self.fail(ThreadWindowError::Invalid.to_string()),
        }
    }

    fn on_page(
        &mut self,
        events: Vec<Verified>,
        bounds: Option<ThreadBounds>,
    ) -> Option<ThreadRead> {
        if !self.has_root(&events) {
            // Hide unavailable history without dropping known tombstones.
            return self.root_missing();
        }
        let root = self.root_id?;
        // Count traversal rows before presentation filtering: the relay may
        // return other content kinds, and aux does not use the page limit.
        let mut replies: Vec<&Verified> = events
            .iter()
            .filter(|event| {
                !AUX.contains(&event.kind_u16())
                    && event.id != root
                    && self.in_channel(event)
                    && thread_reference(event).is_some_and(|r| r.root_id == root)
            })
            .collect();
        replies.sort_by(|a, b| {
            a.created_at_secs()
                .cmp(&b.created_at_secs())
                .then_with(|| a.id.cmp(&b.id))
        });
        let related = self.related(&events);
        let run = self.run.as_mut()?;
        if bounds.is_none()
            && let Some(cursor) = &run.next_cursor
            && replies.iter().any(|event| cursor.cmp_event(event).is_ge())
        {
            return self.fail(NOT_ADVANCING.into());
        }
        run.next_cursor = match &bounds {
            Some(bounds) => bounds.cursor.clone(),
            None => replies
                .last()
                .map(|event| ThreadCursor::of(event))
                .or_else(|| run.next_cursor.take()),
        };
        run.next_pages += 1;
        for event in related {
            run.fetched.insert(event.id, event);
        }
        if over_budget(run.fetched.values()) {
            self.overflow();
            return None;
        }
        // Only signed bounds prove exhaustion. A short legacy page may be
        // filtered after its limit, so only an empty one ends it.
        let more = bounds.map_or(!replies.is_empty(), |bounds| bounds.has_more);
        run.page += 1;
        if more && run.page < run.target_pages {
            return self.begin_page();
        }
        self.finish(more)
    }

    fn finish(&mut self, more: bool) -> Option<ThreadRead> {
        let run = self.run.as_mut()?;
        let fetched = std::mem::take(&mut run.fetched);
        let next_cursor = run.next_cursor.take();
        let next_pages = run.next_pages;
        // A bounded response cannot retract observed evidence: keep known
        // edits, tombstones and live rows a later page omits.
        if !self.extend_remote(fetched.into_values()) {
            return None;
        }
        self.root_unavailable = false;
        self.cursor = next_cursor;
        self.pages = next_pages;
        self.fields = Fields {
            status: ThreadStatus::Ready,
            error: None,
            read_kind: None,
            direction: Some(if self.mode == Mode::Older {
                Direction::Older
            } else {
                Direction::Forward
            }),
            can_load_more: more && self.pages < MAX_PAGES,
            limited: more && self.pages >= MAX_PAGES,
        };
        self.publish();
        self.end_run()
    }

    fn root_missing(&mut self) -> Option<ThreadRead> {
        self.root_unavailable = true;
        self.cursor = None;
        self.pages = 0;
        self.fields.can_load_more = false;
        self.publish();
        self.fail(ROOT_UNAVAILABLE.into())
    }

    fn fail(&mut self, message: String) -> Option<ThreadRead> {
        if self.target_status == Some(TargetStatus::Loading) {
            self.target_status = Some(TargetStatus::Error);
        }
        self.fields.status = ThreadStatus::Error;
        self.fields.error = Some(message);
        self.publish_status();
        self.end_run()
    }

    fn end_run(&mut self) -> Option<ThreadRead> {
        self.run = None;
        self.staged.clear();
        if std::mem::take(&mut self.again) {
            return self.start(true);
        }
        None
    }

    fn purge(&mut self, clear: bool) {
        self.run = None;
        self.selection = None;
        self.admitting = None;
        self.again = false;
        self.staged.clear();
        if self.config.exact {
            self.target_status = Some(TargetStatus::Loading);
        }
        if clear || !self.accessible {
            self.remote.clear();
            self.root_id = None;
            self.cursor = None;
            self.pages = 0;
        }
        self.fields.status = ThreadStatus::Idle;
        self.fields.can_load_more = false;
        self.fields.limited = false;
        self.fields.read_kind = None;
        self.fields.error = Some(
            if self.accessible {
                INTERRUPTED
            } else {
                NO_ACCESS
            }
            .into(),
        );
        self.publish();
    }

    /// Commits `events` as the retained evidence (when `commit`), or clears
    /// the view when they are over budget. Nothing is silently evicted.
    /// Adds `extra` to the remote evidence in place. False, after
    /// [`overflow`](Self::overflow), when the union is over budget.
    fn extend_remote(&mut self, extra: impl IntoIterator<Item = Verified>) -> bool {
        let extra: EventMap = extra
            .into_iter()
            .filter(|event| !self.remote.contains_key(&event.id))
            .map(|event| (event.id, event))
            .collect();
        if union_over_budget(&[&self.remote, &extra]) {
            self.overflow();
            return false;
        }
        self.remote.extend(extra);
        true
    }

    /// The evidence outgrew its budget: forget it and report the limit.
    fn overflow(&mut self) {
        self.run = None;
        self.selection = None;
        self.admitting = None;
        self.again = false;
        self.remote.clear();
        self.staged.clear();
        self.root_id = None;
        self.cursor = None;
        self.pages = 0;
        if self.config.exact {
            self.target_status = Some(TargetStatus::Error);
        }
        self.fields.status = ThreadStatus::Error;
        self.fields.limited = true;
        self.fields.can_load_more = false;
        self.fields.error = Some(MEMORY_LIMIT.into());
        self.publish();
    }

    // ----- retarget -----

    /// A retarget's read came back: the same bounded exact reads as a
    /// target (`readTarget`), checked against the known root.
    fn complete_selection(&mut self, result: Result<Events, ReadError>) -> Completion {
        let mut out = Completion::default();
        let Some(step) = self.selection.as_ref().map(|selection| selection.step) else {
            return out;
        };
        let flow = match result {
            Err(error) if error.kind == ReadErrorKind::Denied => {
                out.denied = true;
                self.revoke();
                ControlFlow::Break(Retarget::Cancelled)
            }
            Err(_) => ControlFlow::Break(Retarget::Error),
            // The exact reader refuses a capped raw response.
            Ok(events) if events.len() >= EVIDENCE_LIMIT => ControlFlow::Break(Retarget::Error),
            Ok(events) => {
                let events: Vec<Verified> = events
                    .iter()
                    .filter(|event| event.kind_u16() != PRODUCT_FEEDBACK)
                    .cloned()
                    .collect();
                match step {
                    ReadStep::Target => self.on_selection_target(events),
                    ReadStep::Overlays => self.on_selection_overlays(events, &mut out.admit),
                    _ => self.on_selection_tombstones(events, &mut out.admit),
                }
            }
        };
        match flow {
            ControlFlow::Continue(read) => out.next = read,
            ControlFlow::Break(outcome) => {
                self.selection = None;
                out.retarget = Some(outcome);
            }
        }
        out.admission = self.admitting.is_some();
        out
    }

    fn issue_selection(
        &mut self,
        step: ReadStep,
        filters: Vec<Filter>,
    ) -> ControlFlow<Retarget, Option<ThreadRead>> {
        let token = self.next_token;
        self.next_token += 1;
        let Some(selection) = self.selection.as_mut() else {
            return ControlFlow::Break(Retarget::Cancelled);
        };
        selection.token = token;
        selection.step = step;
        ControlFlow::Continue(Some(ThreadRead {
            token,
            step,
            filters,
            options: ReadOptions::default(),
        }))
    }

    fn selection_id(&self) -> ControlFlow<Retarget, EventId> {
        match &self.selection {
            Some(selection) => ControlFlow::Continue(selection.id),
            None => ControlFlow::Break(Retarget::Cancelled),
        }
    }

    /// Holds `events` with the candidate so a live deletion of one of
    /// them resolves, without showing an incomplete fold.
    fn stage_selection(
        &mut self,
        events: impl IntoIterator<Item = Verified>,
    ) -> ControlFlow<Retarget> {
        let Some(selection) = self.selection.as_mut() else {
            return ControlFlow::Break(Retarget::Cancelled);
        };
        selection
            .staged
            .extend(events.into_iter().map(|event| (event.id, event)));
        let staged = &selection.staged;
        if union_over_budget(&[&self.remote, &self.staged, staged]) {
            self.overflow();
            return ControlFlow::Break(Retarget::Error);
        }
        ControlFlow::Continue(())
    }

    fn on_selection_target(
        &mut self,
        events: Vec<Verified>,
    ) -> ControlFlow<Retarget, Option<ThreadRead>> {
        // Only checked here: the bare candidate never enters this or any
        // other view's reconciliation before its complete fold does.
        let id = self.selection_id()?;
        let Some(event) = self.selected_in(&events, &id) else {
            return ControlFlow::Break(Retarget::Unavailable);
        };
        let root = thread_reference(&event).map_or(event.id, |reference| reference.root_id);
        if Some(root) != self.root_id {
            return ControlFlow::Break(Retarget::Different);
        }
        self.stage_selection([event])?;
        self.issue_selection(ReadStep::Overlays, vec![overlays_filter(&id)])
    }

    fn on_selection_overlays(
        &mut self,
        events: Vec<Verified>,
        admit: &mut Vec<Verified>,
    ) -> ControlFlow<Retarget, Option<ThreadRead>> {
        let id = self.selection_id()?;
        let related = self.related_to(&events, Some(id));
        self.stage_selection(related)?;
        let Some(selection) = &self.selection else {
            return ControlFlow::Break(Retarget::Cancelled);
        };
        let overlays = overlays_of(&[&self.remote, &selection.staged], &id);
        if overlays.is_empty() {
            return self.finish_selection(admit);
        }
        self.issue_selection(ReadStep::Tombstones, vec![tombstones_filter(&overlays)])
    }

    fn on_selection_tombstones(
        &mut self,
        events: Vec<Verified>,
        admit: &mut Vec<Verified>,
    ) -> ControlFlow<Retarget, Option<ThreadRead>> {
        let id = self.selection_id()?;
        let related = self.related_to(&events, Some(id));
        self.stage_selection(related)?;
        self.finish_selection(admit)
    }

    /// Hands the candidate with its complete overlay closure to the
    /// session, then waits for [`admitted`](Self::admitted).
    fn finish_selection(
        &mut self,
        admit: &mut Vec<Verified>,
    ) -> ControlFlow<Retarget, Option<ThreadRead>> {
        let Some(selection) = &self.selection else {
            return ControlFlow::Break(Retarget::Cancelled);
        };
        admit.extend(fold_evidence(&self.remote, &selection.staged));
        self.admitting = Some(Admitting::Selection);
        ControlFlow::Continue(None)
    }

    /// The session admitted the candidate's fold: it becomes the selected
    /// target if the session kept it and it still folds to a row.
    fn selection_admitted(&mut self, visible: &[Verified]) -> ControlFlow<Retarget, ThreadRead> {
        let Some(selection) = &self.selection else {
            return ControlFlow::Break(Retarget::Cancelled);
        };
        let id = selection.id;
        if !visible.iter().any(|event| event.id == id) {
            return ControlFlow::Break(Retarget::Unavailable);
        }
        let related = self.related_to(visible, Some(id));
        if !self.extend_remote(related) {
            return ControlFlow::Break(Retarget::Error);
        }
        let previous = (
            self.config.message_id,
            self.config.exact,
            self.target_status,
        );
        self.config.message_id = id;
        self.config.exact = true;
        self.target_status = Some(TargetStatus::Ready);
        self.publish();
        if self.target_status != Some(TargetStatus::Ready) {
            // Folded away by a deletion: the earlier selection stays, and
            // so does the real tombstone.
            (
                self.config.message_id,
                self.config.exact,
                self.target_status,
            ) = previous;
            self.publish();
            return ControlFlow::Break(Retarget::Unavailable);
        }
        if self.run.is_none() {
            self.fields.status = ThreadStatus::Ready;
            self.fields.error = None;
            self.fields.read_kind = None;
            self.publish_status();
        }
        ControlFlow::Break(Retarget::Selected)
    }

    // ----- selection -----

    fn in_channel(&self, event: &Verified) -> bool {
        event.has_tag("h", &self.config.channel)
    }

    fn selected_in(&self, events: &[Verified], id: &EventId) -> Option<Verified> {
        events
            .iter()
            .find(|event| event.id == *id && is_content(event) && self.in_channel(event))
            .cloned()
    }

    fn has_root(&self, events: &[Verified]) -> bool {
        events.iter().any(|event| {
            Some(event.id) == self.root_id
                && is_content(event)
                && self.in_channel(event)
                && thread_reference(event).is_none()
        })
    }

    /// The thread's rows in `events`, plus two hops of overlays naming a
    /// known id, so a deletion of a reaction comes along (`threads.ts:119-150`).
    fn related(&self, events: &[Verified]) -> Vec<Verified> {
        let selected = self.config.exact.then_some(self.config.message_id);
        self.related_to(events, selected)
    }

    /// [`related`](Self::related), with `selected` as the exact target.
    fn related_to(&self, events: &[Verified], selected: Option<EventId>) -> Vec<Verified> {
        if self.root_id.is_none() && selected.is_none() {
            return Vec::new();
        }
        let selection = self.selection.as_ref().map(|selection| &selection.staged);
        let mut taken: HashSet<EventId> = HashSet::new();
        let mut result = Vec::new();
        for event in events {
            let kind = event.kind_u16();
            if AUX.contains(&kind) || kind == kinds::THREAD_WINDOW || !self.in_channel(event) {
                continue;
            }
            let belongs = Some(event.id) == self.root_id
                || Some(event.id) == selected
                || (self.root_id.is_some()
                    && thread_reference(event).map(|r| r.root_id) == self.root_id);
            if belongs && taken.insert(event.id) {
                result.push(event.clone());
            }
        }
        let known = |id: &EventId, taken: &HashSet<EventId>| {
            taken.contains(id)
                || self.remote.contains_key(id)
                || self.staged.contains_key(id)
                || selection.is_some_and(|staged| staged.contains_key(id))
                || Some(*id) == selected
        };
        for _ in 0..2 {
            for event in events {
                if !AUX.contains(&event.kind_u16()) || taken.contains(&event.id) {
                    continue;
                }
                if e_targets(event).iter().any(|target| known(target, &taken)) {
                    taken.insert(event.id);
                    result.push(event.clone());
                }
            }
        }
        result
    }

    // ----- output -----

    /// Refolds and republishes (`threads.ts:173-240`).
    fn publish(&mut self) {
        let exact = self.config.exact;
        let operations: &[(Verified, LocalDelivery)] =
            if self.accessible { &self.local } else { &[] };
        let candidates: Vec<Verified> = operations
            .iter()
            .filter(|(event, delivery)| delivery.status != Delivery::Failed || is_content(event))
            .map(|(event, _)| event.clone())
            .collect();
        let local = self.related(&candidates);
        let mut inputs: HashMap<EventId, &Verified> =
            self.remote.iter().map(|(id, event)| (*id, event)).collect();
        for event in &local {
            inputs.insert(event.id, event);
        }
        let deliveries: Vec<(EventId, LocalDelivery)> = operations
            .iter()
            .map(|(event, delivery)| (event.id, delivery.clone()))
            .collect();
        let (rows, _) = self
            .projection
            .reconcile_local(inputs.into_values(), deliveries);
        let rows: &[Arc<MessageRow>] = if self.root_unavailable && !exact {
            &[]
        } else {
            &rows
        };
        let message_id = self.config.message_id;
        let target = if self.target_status == Some(TargetStatus::Ready) {
            rows.iter().find(|row| row.id == message_id).cloned()
        } else {
            None
        };
        if self.target_status == Some(TargetStatus::Ready) && target.is_none() {
            self.target_status = Some(TargetStatus::Unavailable);
        }
        let target_ready = self.target_status == Some(TargetStatus::Ready);
        let readable = |row: &&Arc<MessageRow>| !exact || row.id != message_id || target_ready;
        let replies: Vec<Arc<MessageRow>> = rows
            .iter()
            .filter(readable)
            .filter(|row| {
                Some(row.id) != self.root_id && (!self.root_unavailable || row.id == message_id)
            })
            .cloned()
            .collect();
        let root = if self.root_unavailable {
            None
        } else {
            rows.iter()
                .filter(readable)
                .find(|row| Some(row.id) == self.root_id)
                .cloned()
        };
        let current = &self.current;
        let replies: Arc<[Arc<MessageRow>]> = if current.replies.len() == replies.len()
            && current
                .replies
                .iter()
                .zip(&replies)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        {
            current.replies.clone()
        } else {
            replies.into()
        };
        let tree = if Arc::ptr_eq(&replies, &current.replies) && self.tree_root == self.root_id {
            current.tree.clone()
        } else {
            Arc::new(reply_tree(&replies, self.root_id.as_ref()))
        };
        self.tree_root = self.root_id;
        let next = ThreadSnapshot {
            root,
            replies,
            tree,
            target: if exact { target } else { None },
            ..self.with_fields()
        };
        self.set_snapshot(next);
    }

    /// Republishes status fields over the last fold.
    fn publish_status(&mut self) {
        let next = ThreadSnapshot {
            root: self.current.root.clone(),
            replies: self.current.replies.clone(),
            tree: self.current.tree.clone(),
            target: if self.target_status == Some(TargetStatus::Ready) {
                self.current.target.clone()
            } else {
                None
            },
            ..self.with_fields()
        };
        self.set_snapshot(next);
    }

    fn with_fields(&self) -> ThreadSnapshot {
        let fields = self.fields.clone();
        ThreadSnapshot {
            status: fields.status,
            root: None,
            replies: Arc::new([]),
            tree: Arc::default(),
            error: fields.error,
            can_load_more: fields.can_load_more,
            limited: fields.limited,
            direction: fields.direction,
            read_kind: fields.read_kind,
            target: None,
            target_status: if self.config.exact {
                self.target_status
            } else {
                None
            },
        }
    }

    fn set_snapshot(&mut self, next: ThreadSnapshot) {
        if !next.same(&self.current) {
            self.current = Arc::new(next);
            self.changed = true;
        }
    }
}

fn is_content(event: &Verified) -> bool {
    CONTENT_KINDS.contains(&event.kind_u16())
}

/// Ids named by `e` tags, compared as lowercase hex like the reference.
/// The overlays in `maps` that target `id`, sorted and distinct.
fn overlays_of(maps: &[&EventMap], id: &EventId) -> Vec<EventId> {
    let overlays: BTreeSet<EventId> = maps
        .iter()
        .flat_map(|map| map.values())
        .filter(|event| AUX.contains(&event.kind_u16()) && e_targets(event).contains(id))
        .map(|event| event.id)
        .collect();
    overlays.into_iter().collect()
}

/// A staged target fold as the session admits it: the staged events, and
/// the live deletions of them this view retained while its reads were
/// held, so every view withdraws what they delete (`readTarget`).
fn fold_evidence(remote: &EventMap, staged: &EventMap) -> Vec<Verified> {
    let deletions = remote.values().filter(|event| {
        matches!(event.kind_u16(), kinds::DELETION | kinds::MODERATOR_DELETE)
            && !staged.contains_key(&event.id)
            && e_targets(event).iter().any(|id| staged.contains_key(id))
    });
    staged.values().chain(deletions).cloned().collect()
}

/// [`over_budget`] for the union of `maps`; earlier maps win duplicates.
fn union_over_budget(maps: &[&EventMap]) -> bool {
    over_budget(maps.iter().enumerate().flat_map(|(index, map)| {
        map.values().filter(move |event| {
            !maps[..index]
                .iter()
                .any(|seen| seen.contains_key(&event.id))
        })
    }))
}

/// Over [`MAX_EVENTS`] or over [`MAX_BYTES`] as one JSON array.
fn over_budget<'a>(events: impl Iterator<Item = &'a Verified>) -> bool {
    let mut count = 0;
    let mut bytes = 0;
    for event in events {
        count += 1;
        if count > MAX_EVENTS {
            return true;
        }
        bytes += event.byte_size();
    }
    bytes + count.saturating_sub(1) + 2 > MAX_BYTES
}
