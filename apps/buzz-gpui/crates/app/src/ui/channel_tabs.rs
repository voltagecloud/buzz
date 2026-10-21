//! The channel's workspace tabs (`useChannelTabState.ts`,
//! `useConversationTabs.tsx`): its threads, Channel settings, other
//! conversations and their threads, and channel tools. Threads live in
//! the side pane beside the conversation, or in the main pane after it;
//! everything else is in the side pane. Tabs open from content; there is
//! no blank tab.
//!
//! The state is plain data kept per community and channel, so a channel
//! reopens with the tabs it had; the views are made again on the visit.
//! Each history visit is applied once: Back and Forward switch tabs and
//! never close them.

use nostr::prelude::EventId;
use uuid::Uuid;

/// Me's own thread beside its conversation, and the side selection a
/// timeline action leaves before it selects a thread.
pub const THREAD: &str = "thread";
pub const SETTINGS: &str = "settings";

/// Visits whose selection is kept for Back and Forward.
const VISITS: usize = 50;

/// Where a thread tab sits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabPane {
    /// After the conversation, in the channel's own tab strip.
    Main,
    #[default]
    Side,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadTab {
    pub channel: String,
    /// The message it shows: its root, or a message selected in it.
    pub message: EventId,
    /// The root the opener or a link named. Only a hint: the reader
    /// verifies the real root, kept in `root`.
    pub hint: EventId,
    pub pane: TabPane,
    /// The reader's verified root, which may differ from `hint`.
    pub root: Option<EventId>,
}

impl ThreadTab {
    /// The root it holds, verified or, until then, hinted.
    pub fn root_id(&self) -> EventId {
        self.root.unwrap_or(self.hint)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabKind {
    /// Another conversation, readable and writable here.
    Conversation { channel: String },
    /// A thread of this or another conversation.
    Thread(ThreadTab),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationTab {
    pub id: String,
    pub kind: TabKind,
}

impl ConversationTab {
    pub fn thread(&self) -> Option<&ThreadTab> {
        match &self.kind {
            TabKind::Thread(thread) => Some(thread),
            TabKind::Conversation { .. } => None,
        }
    }

    fn thread_mut(&mut self) -> Option<&mut ThreadTab> {
        match &mut self.kind {
            TabKind::Thread(thread) => Some(thread),
            TabKind::Conversation { .. } => None,
        }
    }

    /// Threads may be moved to the main pane; everything else is beside.
    pub fn pane(&self) -> TabPane {
        self.thread().map(|thread| thread.pane).unwrap_or_default()
    }
}

/// The thread beside a conversation open in Me.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadTarget {
    pub root: EventId,
    /// `root` is any message, revealed in its thread.
    pub exact: bool,
}

/// A channel tool, channel-menu panel or linked panel open as a tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelTab {
    /// The registered panel's key.
    pub panel: &'static str,
    /// The conversation it was opened from.
    pub channel: String,
    /// What it shows: the channel id for tools, else the link.
    pub target: String,
    /// The thread root of the message whose link opened it, as hex.
    pub root: Option<String>,
    /// A channel tool or channel-menu panel, and what replaced one in
    /// its tab (`channelContext`): a timeline link leaves it open.
    pub tool: bool,
}

impl PanelTab {
    /// For a channel tool, which shows its channel.
    pub fn tool(panel: &'static str, channel: &str) -> Self {
        Self {
            panel,
            channel: channel.to_string(),
            target: channel.to_string(),
            root: None,
            tool: true,
        }
    }

    pub fn id(&self) -> String {
        format!("{}:{}", self.panel, self.target)
    }

    /// Details a link opened, which a timeline action replaces; channel
    /// tools stay.
    pub(crate) fn transient(&self) -> bool {
        !self.tool
    }
}

/// A routed thread: the message a route names and the root it hints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routed {
    pub channel: String,
    pub message: EventId,
    pub root: EventId,
}

/// The selection a visit had when it was left.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VisitSelection {
    selected: String,
    main_selected: Option<String>,
    pane_open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabState {
    pub tabs: Vec<ConversationTab>,
    pub panels: Vec<PanelTab>,
    /// The side pane's selected tab.
    pub selected: String,
    /// The main pane's selected thread; `None` is the conversation.
    pub main_selected: Option<String>,
    /// The side pane can be hidden with its tabs kept.
    pub pane_open: bool,
    /// Me's own thread; Messages opens threads as tabs.
    pub thread: Option<ThreadTarget>,
    pub settings: bool,
    /// The last history visit applied, so it is never applied twice.
    visit: Option<String>,
    /// The selection each recent visit had when it was left, oldest
    /// first.
    visits: Vec<(String, VisitSelection)>,
    /// The current visit's selection was already kept as it was left.
    left: bool,
}

impl Default for TabState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            panels: Vec::new(),
            selected: THREAD.into(),
            main_selected: None,
            pane_open: true,
            thread: None,
            settings: false,
            visit: None,
            visits: Vec::new(),
            left: false,
        }
    }
}

/// The id of a new tab for the thread from `root`.
fn thread_id(channel: &str, root: &EventId) -> String {
    format!("thread:{channel}:{}", root.to_hex())
}

impl TabState {
    /// The side pane's tabs in order: Channel settings and Me's thread
    /// lead, then the conversation and thread tabs, then the panels.
    pub fn root_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if self.settings {
            ids.push(SETTINGS.to_string());
        }
        if self.thread.is_some() {
            ids.push(THREAD.to_string());
        }
        ids.extend(self.side_tabs().map(|tab| tab.id.clone()));
        ids.extend(self.panels.iter().map(PanelTab::id));
        ids
    }

    /// Every tab id, in both panes.
    pub fn all_ids(&self) -> Vec<String> {
        let mut ids = self.root_ids();
        ids.extend(self.main_tabs().map(|tab| tab.id.clone()));
        ids
    }

    pub fn side_tabs(&self) -> impl Iterator<Item = &ConversationTab> {
        self.tabs.iter().filter(|tab| tab.pane() == TabPane::Side)
    }

    pub fn main_tabs(&self) -> impl Iterator<Item = &ConversationTab> {
        self.tabs.iter().filter(|tab| tab.pane() == TabPane::Main)
    }

    pub fn tab(&self, id: &str) -> Option<&ConversationTab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    /// The side tab shown: the selected one while it exists, else the
    /// first.
    pub fn selected_tab(&self) -> Option<String> {
        let ids = self.root_ids();
        if ids.contains(&self.selected) {
            Some(self.selected.clone())
        } else {
            ids.into_iter().next()
        }
    }

    /// The main pane's thread shown; `None` shows the conversation.
    pub fn selected_main(&self) -> Option<String> {
        self.main_selected
            .clone()
            .filter(|id| self.main_tabs().any(|tab| tab.id == *id))
    }

    /// The side pane has something to show.
    pub fn has_panel(&self) -> bool {
        self.settings
            || self.thread.is_some()
            || self.side_tabs().next().is_some()
            || !self.panels.is_empty()
    }

    /// The side pane is on screen.
    pub fn showing(&self) -> bool {
        self.pane_open && self.has_panel()
    }

    /// `id` is the tab shown in its pane, and that pane is on screen.
    pub fn is_shown(&self, id: &str) -> bool {
        match self.tab(id).map(ConversationTab::pane) {
            Some(TabPane::Main) => self.selected_main().as_deref() == Some(id),
            _ => self.showing() && self.selected_tab().as_deref() == Some(id),
        }
    }

    pub fn select(&mut self, id: &str) {
        self.selected = id.to_string();
        if !id.is_empty() {
            self.pane_open = true;
        }
    }

    pub fn select_main(&mut self, id: Option<String>) {
        self.main_selected = id;
    }

    /// After `id` closes: when it was the shown tab, the tab now at its
    /// place is shown, or the one before it. `ids` are the tabs before
    /// the close.
    fn after_close(&mut self, id: &str, ids: &[String], shown: Option<&str>) {
        if shown != Some(id) {
            return;
        }
        let index = ids.iter().position(|tab| tab == id).unwrap_or(0);
        let remaining: Vec<&String> = ids.iter().filter(|tab| *tab != id).collect();
        let next = remaining
            .get(index.min(remaining.len().saturating_sub(1)))
            .map(|tab| tab.as_str())
            .unwrap_or_default();
        self.selected = next.to_string();
    }

    /// Closes a conversation or thread tab. In the main pane the
    /// conversation stays, and a closed selected thread gives way to its
    /// neighbour there.
    pub fn close_tab(&mut self, id: &str) {
        if self.tab(id).map(ConversationTab::pane) == Some(TabPane::Main) {
            let main: Vec<String> = self.main_tabs().map(|tab| tab.id.clone()).collect();
            self.tabs.retain(|tab| tab.id != id);
            if self.main_selected.as_deref() == Some(id) {
                let index = main.iter().position(|tab| tab == id).unwrap_or(0);
                let remaining: Vec<&String> = main.iter().filter(|tab| *tab != id).collect();
                self.main_selected = remaining
                    .get(index.min(remaining.len().saturating_sub(1)))
                    .map(|tab| tab.to_string());
            }
            return;
        }
        let ids = self.root_ids();
        let shown = self.selected_tab();
        self.tabs.retain(|tab| tab.id != id);
        self.after_close(id, &ids, shown.as_deref());
    }

    pub fn close_thread(&mut self) {
        let ids = self.root_ids();
        let shown = self.selected_tab();
        self.thread = None;
        self.after_close(THREAD, &ids, shown.as_deref());
    }

    pub fn close_settings(&mut self) {
        let ids = self.root_ids();
        let shown = self.selected_tab();
        self.settings = false;
        self.after_close(SETTINGS, &ids, shown.as_deref());
    }

    pub fn close_panel(&mut self, id: &str) {
        let ids = self.root_ids();
        let shown = self.selected_tab();
        self.panels.retain(|panel| panel.id() != id);
        self.after_close(id, &ids, shown.as_deref());
    }

    /// Opens a panel tab, or selects the one showing the same target. A
    /// link from another conversation retargets that tab to it: true
    /// when it did.
    pub fn open_panel(&mut self, panel: PanelTab) -> bool {
        let id = panel.id();
        let mut retargeted = false;
        match self.panels.iter_mut().find(|open| open.id() == id) {
            Some(open) => {
                // An open tool stays one (`existing`).
                let panel = PanelTab {
                    tool: open.tool || panel.tool,
                    ..panel
                };
                retargeted = *open != panel;
                *open = panel;
            }
            None => self.panels.push(panel),
        }
        self.select(&id);
        retargeted
    }

    /// Tab `to` replaced tool `from` in its place, so it is one too.
    pub fn inherit_tool(&mut self, from: &str, to: &str) {
        if self.panels.iter().any(|tab| tab.id() == from && tab.tool)
            && let Some(tab) = self.panels.iter_mut().find(|tab| tab.id() == to)
        {
            tab.tool = true;
        }
    }

    /// A timeline action replaces the details links opened, never the
    /// channel's tools, and leaves the side selection to what it opens
    /// next (`open(undefined)`).
    pub fn retire_details(&mut self) {
        self.panels.retain(|panel| !panel.transient());
        self.select(THREAD);
    }

    /// Opens a thread: its open tab is selected wherever it was placed,
    /// else a new one is added in `pane` (`withThread`). Returns its id.
    pub fn open_thread_tab(
        &mut self,
        channel: &str,
        message: EventId,
        root: EventId,
        pane: TabPane,
    ) -> String {
        let base = thread_id(channel, &root);
        let existing = self.tabs.iter().position(|tab| {
            tab.thread().is_some_and(|thread| {
                thread.channel == channel
                    && match thread.root {
                        Some(verified) => verified == root,
                        None => tab.id == base,
                    }
            })
        });
        let (id, tab) = match existing {
            Some(index) => {
                let tab = &mut self.tabs[index];
                let id = tab.id.clone();
                let thread = tab.thread_mut().expect("a thread tab");
                // An already-open thread stays wherever it was placed.
                thread.message = message;
                (id, thread.clone())
            }
            None => {
                let id = if self.tabs.iter().any(|tab| tab.id == base) {
                    format!("{base}:{}", Uuid::new_v4())
                } else {
                    base
                };
                let thread = ThreadTab {
                    channel: channel.to_string(),
                    message,
                    hint: root,
                    pane,
                    root: None,
                };
                self.tabs.push(ConversationTab {
                    id: id.clone(),
                    kind: TabKind::Thread(thread.clone()),
                });
                (id, thread)
            }
        };
        match tab.pane {
            TabPane::Main => self.main_selected = Some(id.clone()),
            TabPane::Side => {
                self.selected = id.clone();
                self.pane_open = true;
            }
        }
        id
    }

    fn snapshot(&self) -> VisitSelection {
        VisitSelection {
            selected: self.selected.clone(),
            main_selected: self.main_selected.clone(),
            // An empty pane is not visible, even if its preferred open flag
            // is set; restoring it after a new thread arrived must not
            // reveal that later thread.
            pane_open: self.pane_open && self.has_panel(),
        }
    }

    fn apply(&mut self, saved: VisitSelection) {
        self.selected = saved.selected;
        self.main_selected = saved.main_selected;
        self.pane_open = saved.pane_open;
    }

    /// Keeps the current visit's selection, at most [`VISITS`] of them.
    fn remember(&mut self) {
        let Some(visit) = self.visit.clone() else {
            return;
        };
        let snapshot = self.snapshot();
        match self.visits.iter_mut().find(|(key, _)| *key == visit) {
            Some((_, saved)) => *saved = snapshot,
            None => self.visits.push((visit, snapshot)),
        }
        let excess = self.visits.len().saturating_sub(VISITS);
        self.visits.drain(..excess);
    }

    /// An action that starts a new visit (opening a thread) keeps the
    /// current one's selection first, before it changes any.
    pub fn leave_visit(&mut self) {
        if self.visit.is_some() && !self.left {
            self.remember();
            self.left = true;
        }
    }

    /// Applies a history visit once. A thread visit selects its thread;
    /// another restores the tabs it last had. Nothing is ever closed.
    pub fn enter_visit(&mut self, visit: &str, routed: Option<&Routed>, pane: TabPane) {
        if self.visit.as_deref() == Some(visit) {
            return;
        }
        if !self.left {
            self.remember();
        }
        self.visit = Some(visit.to_string());
        self.left = false;
        if let Some(routed) = routed {
            self.open_thread_tab(&routed.channel, routed.message, routed.root, pane);
            return;
        }
        let saved = self
            .visits
            .iter()
            .find(|(key, _)| key == visit)
            .map(|(_, saved)| saved.clone());
        match saved {
            Some(saved) => self.apply(saved),
            None => self.main_selected = None,
        }
    }

    /// A hint named an existing tab, but its verified reader found the
    /// message in another thread: the tab keeps `kept` and its reply,
    /// and a provisional tab in the same pane resolves the link.
    pub fn divert_thread(&mut self, id: &str, requested: EventId, kept: EventId) {
        let Some(tab) = self.tab(id) else {
            return;
        };
        let Some(thread) = tab.thread().filter(|thread| thread.message == requested) else {
            return;
        };
        let (channel, pane) = (thread.channel.clone(), thread.pane);
        let visible = match pane {
            TabPane::Main => self.main_selected.as_deref() == Some(id),
            TabPane::Side => self.selected == id && self.pane_open,
        };
        let before = (self.snapshot(), self.pane_open);
        if let Some(thread) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .and_then(ConversationTab::thread_mut)
        {
            thread.message = kept;
        }
        self.open_thread_tab(&channel, requested, requested, pane);
        if !visible {
            let (selection, pane_open) = before;
            self.apply(selection);
            self.pane_open = pane_open;
        }
    }

    /// Moves a thread tab to `index` within `pane`'s own order. Its
    /// identity and view stay; only its place changes, and selection
    /// follows it.
    pub fn move_tab(&mut self, id: &str, pane: TabPane, index: usize) {
        let Some(at) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        if self.tabs[at].thread().is_none() {
            return;
        }
        let mut moving = self.tabs.remove(at);
        let left = moving.pane() != pane;
        if let Some(thread) = moving.thread_mut() {
            thread.pane = pane;
        }
        let before = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.pane() == pane)
            .nth(index)
            .map(|(position, _)| position);
        let position = before.unwrap_or(self.tabs.len());
        self.tabs.insert(position, moving);
        match pane {
            TabPane::Main => {
                self.main_selected = Some(id.to_string());
                if left && self.selected == id {
                    self.selected = String::new();
                }
            }
            TabPane::Side => {
                self.selected = id.to_string();
                self.pane_open = true;
                if left && self.main_selected.as_deref() == Some(id) {
                    self.main_selected = None;
                }
            }
        }
    }

    /// The reader of tab `id` verified its root. A tab opened from a
    /// wrong hint merges into the tab already holding that thread, but
    /// late evidence never overrides a newer selection.
    pub fn resolve_thread_root(&mut self, id: &str, root: EventId) {
        let Some(tab) = self.tab(id) else {
            return;
        };
        let Some(thread) = tab.thread().filter(|thread| thread.root != Some(root)) else {
            return;
        };
        let (channel, message, pane) = (thread.channel.clone(), thread.message, thread.pane);
        let owner = self
            .tabs
            .iter()
            .position(|item| {
                item.id != id
                    && item.thread().is_some_and(|other| {
                        other.channel == channel
                            && match other.root {
                                Some(verified) => verified == root,
                                None => item.id == thread_id(&channel, &root),
                            }
                    })
            })
            .map(|index| self.tabs[index].id.clone());
        let Some(owner) = owner else {
            if let Some(thread) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == id)
                .and_then(ConversationTab::thread_mut)
            {
                thread.root = Some(root);
            }
            return;
        };
        let selected = match pane {
            TabPane::Main => self.main_selected.as_deref() == Some(id),
            TabPane::Side => self.selected == id,
        };
        let foreground = selected && (pane == TabPane::Main || self.pane_open);
        self.tabs.retain(|tab| tab.id != id);
        let owner_pane = self
            .tab(&owner)
            .map(ConversationTab::pane)
            .unwrap_or_default();
        if foreground
            && let Some(thread) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == owner)
                .and_then(ConversationTab::thread_mut)
        {
            thread.message = message;
            thread.root = Some(root);
        }
        if !foreground {
            if selected {
                self.selected = match owner_pane {
                    TabPane::Side => owner,
                    TabPane::Main => String::new(),
                };
            }
            return;
        }
        match owner_pane {
            TabPane::Main => {
                if self.selected == id {
                    self.selected = String::new();
                }
                self.main_selected = Some(owner);
            }
            TabPane::Side => {
                if self.main_selected.as_deref() == Some(id) {
                    self.main_selected = None;
                }
                self.selected = owner;
                self.pane_open = true;
            }
        }
    }

    /// Opens Me's thread beside its conversation.
    pub fn open_thread(&mut self, thread: ThreadTarget) {
        self.thread = Some(thread);
        self.select(THREAD);
    }

    pub fn open_settings(&mut self) {
        self.settings = true;
        self.select(SETTINGS);
    }

    /// The header toggle: hides the pane, or shows it again with its
    /// tabs as they were; `side` is a launcher's panel in the pane. With
    /// nothing to show the toggle is disabled.
    pub fn toggle_pane(&mut self, side: bool) {
        self.pane_open = !(self.pane_open && (self.has_panel() || side));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    fn thread() -> ThreadTarget {
        ThreadTarget {
            root: event(1),
            exact: false,
        }
    }

    fn conversation(id: &str) -> ConversationTab {
        ConversationTab {
            id: format!("conversation:{id}"),
            kind: TabKind::Conversation { channel: id.into() },
        }
    }

    fn id(root: u8) -> String {
        thread_id("alpha", &event(root))
    }

    fn tab_thread(state: &TabState, id: &str) -> ThreadTab {
        state.tab(id).unwrap().thread().unwrap().clone()
    }

    fn ids(state: &TabState) -> Vec<String> {
        state.tabs.iter().map(|tab| tab.id.clone()).collect()
    }

    fn routed(message: u8, root: u8) -> Routed {
        Routed {
            channel: "alpha".into(),
            message: event(message),
            root: event(root),
        }
    }

    #[test]
    fn settings_and_the_thread_lead_the_tabs() {
        let mut state = TabState::default();
        assert!(!state.has_panel());
        state.tabs.push(conversation("a"));
        state.open_thread(thread());
        state.open_settings();
        state.open_panel(PanelTab::tool("buzz.todos/todos", "c"));
        assert_eq!(
            state.root_ids(),
            ["settings", "thread", "conversation:a", "buzz.todos/todos:c"]
        );
        assert_eq!(state.selected_tab().as_deref(), Some("buzz.todos/todos:c"));
    }

    #[test]
    fn closing_the_shown_tab_shows_its_neighbour() {
        let mut state = TabState::default();
        for id in ["1", "2", "3"] {
            state.tabs.push(conversation(id));
        }
        state.select("conversation:2");
        state.close_tab("conversation:2");
        assert_eq!(state.selected_tab().as_deref(), Some("conversation:3"));
        state.close_tab("conversation:3");
        assert_eq!(state.selected_tab().as_deref(), Some("conversation:1"));
        // Closing a tab that isn't shown keeps the selection.
        state.tabs.push(conversation("4"));
        state.select("conversation:4");
        state.close_tab("conversation:1");
        assert_eq!(state.selected_tab().as_deref(), Some("conversation:4"));
        state.close_tab("conversation:4");
        assert_eq!(state.selected_tab(), None);
        assert!(!state.showing());
    }

    #[test]
    fn closing_a_main_thread_keeps_the_conversation_and_its_neighbours() {
        let mut state = TabState::default();
        for root in 1..=3 {
            state.open_thread_tab("alpha", event(root), event(root), TabPane::Main);
        }
        state.select_main(Some(id(2)));
        state.close_tab(&id(2));
        assert_eq!(state.selected_main(), Some(id(3)));
        state.close_tab(&id(3));
        assert_eq!(state.selected_main(), Some(id(1)));
        state.close_tab(&id(1));
        assert_eq!(state.selected_main(), None);
        assert!(state.tabs.is_empty());
    }

    #[test]
    fn the_toggle_hides_and_shows_the_tabs_it_has() {
        let mut state = TabState::default();
        // Nothing to show: no blank tab appears.
        state.toggle_pane(false);
        assert!(!state.showing());
        assert!(state.root_ids().is_empty());
        state.pane_open = true;
        state.open_thread(thread());
        state.toggle_pane(false);
        assert!(!state.showing());
        assert_eq!(state.root_ids(), ["thread"]);
        state.toggle_pane(false);
        assert!(state.showing());
        // A launcher's side panel alone is something to hide.
        state.close_thread();
        state.toggle_pane(true);
        assert!(!state.pane_open);
        state.toggle_pane(true);
        assert!(state.pane_open);
    }

    #[test]
    fn opening_a_thread_selects_it_and_keeps_one_tab_per_root() {
        let mut state = TabState {
            pane_open: false,
            ..Default::default()
        };
        let first = state.open_thread_tab("alpha", event(11), event(1), TabPane::Side);
        assert_eq!(first, id(1));
        assert_eq!(state.selected, id(1));
        assert!(state.pane_open);
        state.open_thread_tab("alpha", event(2), event(2), TabPane::Side);
        state.open_thread_tab("alpha", event(12), event(1), TabPane::Side);
        assert_eq!(ids(&state), [id(1), id(2)]);
        assert_eq!(tab_thread(&state, &id(1)).message, event(12));
        assert_eq!(tab_thread(&state, &id(2)).message, event(2));
        assert_eq!(state.selected, id(1));
    }

    #[test]
    fn a_timeline_action_retires_linked_details_but_not_tools() {
        let mut state = TabState::default();
        state.open_panel(PanelTab::tool("buzz.todos/todos", "alpha"));
        state.open_panel(PanelTab {
            panel: "buzz.github/github",
            channel: "alpha".into(),
            target: "https://github.com/block/buzz".into(),
            root: None,
            tool: false,
        });
        state.pane_open = false;
        state.retire_details();
        assert_eq!(state.panels, [PanelTab::tool("buzz.todos/todos", "alpha")]);
        assert_eq!(state.selected, THREAD);
        assert!(state.pane_open);
    }

    #[test]
    fn moving_a_thread_reorders_it_within_a_pane_and_across_panes() {
        let mut state = TabState::default();
        for root in 1..=3 {
            state.open_thread_tab("alpha", event(root), event(root), TabPane::Side);
        }
        state.move_tab(&id(3), TabPane::Side, 0);
        assert_eq!(ids(&state), [id(3), id(1), id(2)]);
        assert_eq!(state.selected, id(3));
        state.move_tab(&id(1), TabPane::Main, 0);
        assert_eq!(tab_thread(&state, &id(1)).pane, TabPane::Main);
        assert_eq!(state.main_selected, Some(id(1)));
        // Reopening a moved thread selects it where it now lives.
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Side);
        assert_eq!(tab_thread(&state, &id(1)).pane, TabPane::Main);
        state.move_tab(&id(1), TabPane::Side, 1);
        assert_eq!(ids(&state), [id(3), id(1), id(2)]);
        assert_eq!(state.main_selected, None);
        assert_eq!(state.selected, id(1));
    }

    #[test]
    fn a_tab_opened_from_a_wrong_hint_merges_into_the_verified_threads_tab() {
        let mut state = TabState::default();
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Side);
        state.move_tab(&id(1), TabPane::Main, 0);
        state.open_thread_tab("alpha", event(5), event(9), TabPane::Side);
        assert_eq!(state.tabs.len(), 2);
        state.resolve_thread_root(&id(9), event(1));
        assert_eq!(ids(&state), [id(1)]);
        let merged = tab_thread(&state, &id(1));
        assert_eq!(merged.message, event(5));
        assert_eq!(merged.pane, TabPane::Main);
        assert_eq!(merged.root, Some(event(1)));
        assert_eq!(state.main_selected, Some(id(1)));
        // Later openings by either identity select the same tab.
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Side);
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn late_root_confirmation_keeps_a_newer_selection_and_a_closed_pane() {
        for pane in [TabPane::Side, TabPane::Main] {
            let mut state = TabState::default();
            state.open_thread_tab("alpha", event(1), event(1), TabPane::Side);
            state.resolve_thread_root(&id(1), event(1));
            state.open_thread_tab("alpha", event(5), event(9), pane);
            state.open_thread_tab("alpha", event(7), event(7), pane);
            state.pane_open = false;
            state.resolve_thread_root(&id(9), event(1));
            assert!(!state.pane_open);
            let shown = match pane {
                TabPane::Main => state.main_selected.clone(),
                TabPane::Side => Some(state.selected.clone()),
            };
            assert_eq!(shown, Some(id(7)));
            assert_eq!(tab_thread(&state, &id(1)).message, event(1));
        }
    }

    #[test]
    fn a_selected_hidden_provisional_tab_merges_without_switching_the_main_pane() {
        let mut state = TabState::default();
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Main);
        state.resolve_thread_root(&id(1), event(1));
        state.open_thread_tab("alpha", event(7), event(7), TabPane::Main);
        state.open_thread_tab("alpha", event(5), event(9), TabPane::Side);
        state.pane_open = false;
        state.resolve_thread_root(&id(9), event(1));
        assert_eq!(state.main_selected, Some(id(7)));
        assert!(!state.pane_open);
    }

    #[test]
    fn a_wrong_root_diversion_keeps_its_pane_and_resolves_into_the_owner() {
        for pane in [TabPane::Side, TabPane::Main] {
            let other = match pane {
                TabPane::Side => TabPane::Main,
                TabPane::Main => TabPane::Side,
            };
            let mut state = TabState::default();
            state.open_thread_tab("alpha", event(1), event(1), pane);
            state.resolve_thread_root(&id(1), event(1));
            state.open_thread_tab("alpha", event(2), event(2), other);
            state.resolve_thread_root(&id(2), event(2));
            state.open_thread_tab("alpha", event(22), event(1), pane);
            state.divert_thread(&id(1), event(22), event(1));
            let kept = tab_thread(&state, &id(1));
            assert_eq!(
                (kept.message, kept.root, kept.pane),
                (event(1), Some(event(1)), pane)
            );
            let provisional = tab_thread(&state, &id(22));
            assert_eq!((provisional.message, provisional.pane), (event(22), pane));
            let shown = match pane {
                TabPane::Main => state.main_selected.clone(),
                TabPane::Side => Some(state.selected.clone()),
            };
            assert_eq!(shown, Some(id(22)));
            // An already-open verified destination keeps its own place.
            state.resolve_thread_root(&id(22), event(2));
            assert_eq!(state.tabs.len(), 2);
            let shown = match other {
                TabPane::Main => state.main_selected.clone(),
                TabPane::Side => Some(state.selected.clone()),
            };
            assert_eq!(shown, Some(id(2)));
            let owner = tab_thread(&state, &id(2));
            assert_eq!((owner.message, owner.pane), (event(22), other));
            let tabs = state.tabs.clone();
            state.divert_thread(&id(1), event(22), event(1));
            assert_eq!(state.tabs, tabs);
        }
    }

    #[test]
    fn verified_identity_takes_precedence_over_a_tabs_hint_id() {
        let mut state = TabState::default();
        state.open_thread_tab("alpha", event(22), event(1), TabPane::Side);
        state.resolve_thread_root(&id(1), event(2));
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Side);
        assert_eq!(state.tabs.len(), 2);
        assert_ne!(state.tabs[0].id, state.tabs[1].id);
        let first = state.tabs[0].thread().unwrap();
        assert_eq!((first.message, first.root), (event(22), Some(event(2))));
    }

    #[test]
    fn revisiting_a_history_entry_restores_its_selection_without_closing_threads() {
        let mut state = TabState::default();
        state.enter_visit("one:channel", None, TabPane::Side);
        state.select("detail");
        state.pane_open = false;
        // Opening a thread changes selection before its visit arrives.
        state.leave_visit();
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Side);
        let thread = routed(1, 1);
        state.enter_visit("two:thread", Some(&thread), TabPane::Side);
        assert_eq!(state.selected, id(1));
        state.enter_visit("one:channel", None, TabPane::Side);
        assert_eq!(state.selected, "detail");
        assert!(!state.pane_open);
        assert_eq!(state.tabs.len(), 1);
        // Entering the same visit again changes nothing.
        state.select("other");
        state.enter_visit("one:channel", None, TabPane::Side);
        assert_eq!(state.selected, "other");
        state.enter_visit("two:thread", Some(&thread), TabPane::Side);
        assert_eq!(state.selected, id(1));
    }

    #[test]
    fn back_restores_an_empty_side_pane_as_hidden_keeping_the_later_thread() {
        let mut state = TabState::default();
        state.enter_visit("channel", None, TabPane::Side);
        state.leave_visit();
        state.enter_visit("link", Some(&routed(1, 1)), TabPane::Side);
        assert!(state.pane_open);
        state.enter_visit("channel", None, TabPane::Side);
        assert!(!state.pane_open);
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn a_new_channel_visit_shows_the_conversation() {
        let mut state = TabState::default();
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Main);
        state.enter_visit("one:channel", None, TabPane::Side);
        assert_eq!(state.selected_main(), None);
        assert_eq!(state.main_tabs().count(), 1);
    }

    #[test]
    fn only_the_latest_visits_are_kept() {
        let mut state = TabState::default();
        for visit in 0..VISITS + 5 {
            state.enter_visit(&visit.to_string(), None, TabPane::Side);
        }
        state.leave_visit();
        assert_eq!(state.visits.len(), VISITS);
        assert_eq!(state.visits[0].0, "5");
    }

    #[test]
    fn new_threads_follow_the_pane_given_and_open_ones_stay() {
        let mut state = TabState::default();
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Side);
        state.open_thread_tab("alpha", event(2), event(2), TabPane::Main);
        assert_eq!(tab_thread(&state, &id(2)).pane, TabPane::Main);
        state.open_thread_tab("alpha", event(1), event(1), TabPane::Main);
        assert_eq!(tab_thread(&state, &id(1)).pane, TabPane::Side);
        state.enter_visit("link", Some(&routed(3, 3)), TabPane::Main);
        assert_eq!(tab_thread(&state, &id(3)).pane, TabPane::Main);
    }
}
