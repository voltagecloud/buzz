//! The session's sidebar preferences store
//! (`sidebar-preferences-store.ts`): one confirmed copy of the kind-30078
//! preferences, the moves and sorts shown before the relay confirms them,
//! and the failures the sidebar offers to retry or dismiss.
//!
//! Pure state. The session runs the reads and writes one after another
//! and reports back through the `finish_*` methods; each takes the ticket
//! its `begin_*` call returned, so a result that started before
//! [`PreferencesStore::clear`] is dropped.

use crate::sidebar::{NewSection, SidebarGroups, SidebarPreferences, SortMode};
use std::collections::{BTreeMap, HashMap};

const MOVES_UNAVAILABLE: &str =
    "Sidebar group moves are unavailable; refresh saved sidebar preferences before retrying";

/// Where a move puts a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    Starred,
    /// A saved group, or `None` for its default section (Channels or
    /// Direct messages).
    Section(Option<String>),
    /// A new group, created with the move.
    Create(NewSection),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveIntent {
    pub id: u64,
    pub channel_id: String,
    pub destination: Destination,
}

/// A move shown in the sidebar: still saving, or failed with `error`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveState {
    pub intent: MoveIntent,
    pub pending: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortFailure {
    /// The sort key: `starred`, `channels`, `forums`, `dms` or
    /// `section:<id>`.
    pub group: String,
    pub mode: SortMode,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PreferencesStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    Error(String),
}

/// What the sidebar shows, published in the session snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarState {
    pub status: PreferencesStatus,
    /// Confirmed preferences with the unconfirmed moves and sorts applied,
    /// or a display-only copy saved at the last startup.
    pub data: Option<SidebarPreferences>,
    /// `data` is the saved startup copy: nothing can be written yet.
    pub cached: bool,
    /// Moves and stars can be saved.
    pub writable: bool,
    pub sort_errors: Vec<SortFailure>,
    pub moves: Vec<MoveState>,
    /// DMs the viewer removed from Messages on this device.
    pub hidden_dms: Vec<String>,
    /// Conversations placed in Me, which leave the Messages sidebar.
    pub me_channels: Vec<String>,
}

impl SidebarState {
    pub fn sort_writable(&self) -> bool {
        self.data.is_some() && !self.cached
    }

    pub fn mute_writable(&self) -> bool {
        self.data.is_some() && !self.cached
    }

    pub fn remove_section_writable(&self) -> bool {
        self.writable && !self.cached
    }

    /// Something is still being saved.
    pub fn saving(&self) -> bool {
        self.moves.iter().any(|m| m.pending)
    }
}

/// Fences a result to the store generation and mutation it started in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    generation: u64,
    id: u64,
}

#[derive(Debug, Default)]
pub struct PreferencesStore {
    generation: u64,
    /// Bumped by every write, so a read that overlapped one is dropped.
    mutation: u64,
    confirmed: Option<SidebarPreferences>,
    cached: Option<SidebarPreferences>,
    read_failure: Option<String>,
    /// The mutation a running read started at.
    reading: Option<u64>,
    next_move: u64,
    pending_moves: BTreeMap<u64, MoveIntent>,
    failed_moves: BTreeMap<String, (MoveIntent, String)>,
    latest_move: HashMap<String, u64>,
    next_sort: u64,
    pending_sorts: BTreeMap<u64, (String, SortMode)>,
    latest_sort: HashMap<String, u64>,
    failed_sorts: BTreeMap<String, SortFailure>,
}

impl PreferencesStore {
    /// Moves, stars and section removal need a confirmed read.
    pub fn writable(&self) -> bool {
        self.confirmed.is_some() && self.read_failure.is_none()
    }

    pub fn has_pending_moves(&self) -> bool {
        !self.pending_moves.is_empty()
    }

    pub fn reading(&self) -> bool {
        self.reading.is_some()
    }

    pub fn confirmed(&self) -> Option<&SidebarPreferences> {
        self.confirmed.as_ref()
    }

    fn ticket(&self, id: u64) -> Ticket {
        Ticket {
            generation: self.generation,
            id,
        }
    }

    fn live(&self, ticket: Ticket) -> bool {
        ticket.generation == self.generation
    }

    /// Shows preferences saved at the last startup until the first read
    /// confirms them. Never a base for writes.
    pub fn restore_cached(&mut self, data: SidebarPreferences) {
        if self.confirmed.is_none() {
            self.cached = Some(data);
        }
    }

    /// Starts a read, unless one runs or moves are still saving (the read
    /// then waits for them, `refresh` in the reference).
    pub fn begin_read(&mut self) -> Option<Ticket> {
        if self.reading.is_some() || self.has_pending_moves() {
            return None;
        }
        self.reading = Some(self.mutation);
        Some(self.ticket(self.mutation))
    }

    /// A read made stale by a write leaves the state as the write left it.
    /// Returns whether the result was taken.
    pub fn finish_read(
        &mut self,
        ticket: Ticket,
        result: Result<SidebarPreferences, String>,
    ) -> bool {
        if !self.live(ticket) || self.reading != Some(ticket.id) {
            return false;
        }
        self.reading = None;
        if self.mutation != ticket.id {
            return false;
        }
        match result {
            Ok(data) => {
                self.read_failure = None;
                self.confirmed = Some(data);
                self.cached = None;
            }
            Err(error) => self.read_failure = Some(error),
        }
        true
    }

    /// Shows the move at once. Fails when preferences are not writable;
    /// a failed move for the channel then shows the error too.
    pub fn begin_move(
        &mut self,
        channel_id: &str,
        destination: Destination,
    ) -> Result<(Ticket, MoveIntent), String> {
        if !self.writable() {
            if let Some((_, error)) = self.failed_moves.get_mut(channel_id) {
                *error = MOVES_UNAVAILABLE.into();
            }
            return Err(MOVES_UNAVAILABLE.into());
        }
        self.next_move += 1;
        let intent = MoveIntent {
            id: self.next_move,
            channel_id: channel_id.to_owned(),
            destination,
        };
        self.pending_moves.insert(intent.id, intent.clone());
        self.latest_move.insert(channel_id.to_owned(), intent.id);
        self.failed_moves.remove(channel_id);
        self.mutation += 1;
        Ok((self.ticket(intent.id), intent))
    }

    /// The saved groups (absent when only stars were written) and stars.
    /// A failure keeps its retry state only for the channel's latest move.
    pub fn finish_move(
        &mut self,
        ticket: Ticket,
        result: Result<(Option<SidebarGroups>, Vec<String>), String>,
    ) -> Result<(), String> {
        if !self.live(ticket) {
            return Err("Sidebar group moves are unavailable".into());
        }
        self.mutation += 1;
        let Some(intent) = self.pending_moves.remove(&ticket.id) else {
            return Err("Sidebar group moves are unavailable".into());
        };
        match result {
            Ok((groups, starred)) => {
                let Some(confirmed) = &mut self.confirmed else {
                    return Err("Sidebar group moves are unavailable".into());
                };
                if let Some(groups) = groups {
                    confirmed.sections = groups.sections;
                    confirmed.assignments = groups.assignments;
                }
                confirmed.starred = starred;
                Ok(())
            }
            Err(error) => {
                if self.latest_move.get(&intent.channel_id) == Some(&intent.id) {
                    self.failed_moves
                        .insert(intent.channel_id.clone(), (intent, error.clone()));
                }
                Err(error)
            }
        }
    }

    /// The failed move's destination, to move again.
    pub fn retry_move(&mut self, channel_id: &str) -> Result<(Ticket, MoveIntent), String> {
        let Some((failed, _)) = self.failed_moves.get(channel_id) else {
            return Err("No failed move to retry".into());
        };
        let destination = failed.destination.clone();
        self.begin_move(channel_id, destination)
    }

    pub fn dismiss_move(&mut self, channel_id: &str) {
        self.failed_moves.remove(channel_id);
    }

    pub fn begin_sort(&mut self, group: &str, mode: SortMode) -> Result<Ticket, String> {
        if self.confirmed.is_none() {
            return Err("Sidebar sorting is read-only in this host".into());
        }
        self.next_sort += 1;
        let id = self.next_sort;
        self.pending_sorts.insert(id, (group.to_owned(), mode));
        self.latest_sort.insert(group.to_owned(), id);
        self.failed_sorts.remove(group);
        self.mutation += 1;
        Ok(self.ticket(id))
    }

    pub fn finish_sort(
        &mut self,
        ticket: Ticket,
        result: Result<BTreeMap<String, SortMode>, String>,
    ) {
        if !self.live(ticket) {
            return;
        }
        self.mutation += 1;
        let Some((group, mode)) = self.pending_sorts.remove(&ticket.id) else {
            return;
        };
        match result {
            Ok(sort) => {
                if let Some(confirmed) = &mut self.confirmed {
                    confirmed.sort = sort;
                }
            }
            Err(error) => {
                if self.latest_sort.get(&group) == Some(&ticket.id) {
                    self.failed_sorts
                        .insert(group.clone(), SortFailure { group, mode, error });
                }
            }
        }
    }

    pub fn dismiss_sort(&mut self, group: &str) {
        self.failed_sorts.remove(group);
    }

    pub fn begin_mute(&mut self) -> Result<Ticket, String> {
        if self.confirmed.is_none() {
            return Err("Sidebar mutes are unavailable in this host".into());
        }
        Ok(self.ticket(0))
    }

    pub fn finish_mute(&mut self, ticket: Ticket, muted: Vec<String>) -> Result<(), String> {
        match &mut self.confirmed {
            Some(confirmed) if self.generation == ticket.generation => {
                self.mutation += 1;
                confirmed.muted = muted;
                Ok(())
            }
            _ => Err("Sidebar mutes are unavailable".into()),
        }
    }

    /// The section ids a removal's sort reset is checked against.
    pub fn begin_remove_section(
        &mut self,
        section_id: &str,
    ) -> Result<(Ticket, Vec<String>), String> {
        let Some(confirmed) = self
            .confirmed
            .as_ref()
            .filter(|_| self.read_failure.is_none())
        else {
            return Err("Sidebar section removal is unavailable".into());
        };
        if !confirmed.sections.iter().any(|s| s.id == section_id) {
            return Err("Sidebar section no longer exists".into());
        }
        let ids = confirmed.sections.iter().map(|s| s.id.clone()).collect();
        self.mutation += 1;
        Ok((self.ticket(0), ids))
    }

    /// The section's sort was reset to A–Z (`sort`), then the section
    /// was removed (`groups`). Either step may have failed.
    pub fn finish_remove_section(
        &mut self,
        ticket: Ticket,
        sort: Option<BTreeMap<String, SortMode>>,
        groups: Result<SidebarGroups, String>,
    ) -> Result<(), String> {
        if !self.live(ticket) {
            return Err("Sidebar section removal is unavailable".into());
        }
        self.mutation += 1;
        let Some(confirmed) = &mut self.confirmed else {
            return Err("Sidebar section removal is unavailable".into());
        };
        if let Some(sort) = sort {
            confirmed.sort = sort;
        }
        let groups = groups?;
        confirmed.sections = groups.sections;
        confirmed.assignments = groups.assignments;
        Ok(())
    }

    /// Forgets everything, e.g. when the viewer or relay changes.
    pub fn clear(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            next_move: self.next_move,
            next_sort: self.next_sort,
            ..Self::default()
        };
    }

    /// Confirmed data with unconfirmed moves and sorts on top.
    fn view(&self) -> Option<SidebarPreferences> {
        let Some(confirmed) = &self.confirmed else {
            return self.cached.clone();
        };
        let mut data = confirmed.clone();
        for (group, mode) in self.pending_sorts.values() {
            match mode {
                SortMode::Alpha => data.sort.remove(group),
                SortMode::Recent => data.sort.insert(group.clone(), *mode),
            };
        }
        for intent in self.pending_moves.values() {
            let channel = &intent.channel_id;
            let target = match &intent.destination {
                Destination::Starred => {
                    if !data.starred.contains(channel) {
                        data.starred.push(channel.clone());
                    }
                    continue;
                }
                Destination::Section(section) => section.clone(),
                Destination::Create(new) => {
                    if !data.sections.iter().any(|s| s.id == new.id) {
                        let order =
                            data.sections.iter().map(|s| s.order).fold(-1.0, f64::max) + 1.0;
                        data.sections.push(crate::sidebar::SavedSection {
                            id: new.id.clone(),
                            name: new.name.trim().to_owned(),
                            icon: None,
                            order,
                        });
                    }
                    Some(new.id.clone())
                }
            };
            data.starred.retain(|id| id != channel);
            match target {
                Some(section) => data.assignments.insert(channel.clone(), section),
                None => data.assignments.remove(channel),
            };
        }
        Some(data)
    }

    pub fn state(&self, hidden_dms: Vec<String>) -> SidebarState {
        let status = match (&self.read_failure, self.reading.is_some()) {
            (Some(error), _) => PreferencesStatus::Error(error.clone()),
            (None, true) => PreferencesStatus::Loading,
            (None, false) if self.confirmed.is_some() => PreferencesStatus::Ready,
            _ => PreferencesStatus::Idle,
        };
        let moves = self
            .pending_moves
            .values()
            .map(|intent| MoveState {
                intent: intent.clone(),
                pending: true,
                error: None,
            })
            .chain(self.failed_moves.values().map(|(intent, error)| MoveState {
                intent: intent.clone(),
                pending: false,
                error: Some(error.clone()),
            }))
            .collect();
        SidebarState {
            status,
            data: self.view(),
            cached: self.confirmed.is_none() && self.cached.is_some(),
            writable: self.writable(),
            sort_errors: self.failed_sorts.values().cloned().collect(),
            moves,
            hidden_dms,
            me_channels: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::SavedSection;

    fn section(id: &str, order: f64) -> SavedSection {
        SavedSection {
            id: id.into(),
            name: id.to_uppercase(),
            icon: None,
            order,
        }
    }

    fn ready() -> PreferencesStore {
        let mut store = PreferencesStore::default();
        let ticket = store.begin_read().unwrap();
        let data = SidebarPreferences {
            sections: vec![section("a", 0.0)],
            starred: vec!["x".into()],
            ..Default::default()
        };
        assert!(store.finish_read(ticket, Ok(data)));
        store
    }

    #[test]
    fn reads_become_ready_or_fail() {
        let mut store = PreferencesStore::default();
        assert_eq!(store.state(vec![]).status, PreferencesStatus::Idle);
        let ticket = store.begin_read().unwrap();
        assert!(store.begin_read().is_none(), "one read at a time");
        assert_eq!(store.state(vec![]).status, PreferencesStatus::Loading);
        store.finish_read(ticket, Err("Preference query: down".into()));
        let state = store.state(vec![]);
        assert_eq!(
            state.status,
            PreferencesStatus::Error("Preference query: down".into())
        );
        assert!(!state.writable);
        let ready = ready().state(vec![]);
        assert_eq!(ready.status, PreferencesStatus::Ready);
        assert!(ready.writable);
    }

    #[test]
    fn cached_preferences_show_but_never_write() {
        let mut store = PreferencesStore::default();
        store.restore_cached(SidebarPreferences {
            starred: vec!["x".into()],
            ..Default::default()
        });
        let state = store.state(vec![]);
        assert!(state.cached && !state.writable && !state.sort_writable());
        assert_eq!(state.data.unwrap().starred, vec!["x".to_string()]);
        assert!(store.begin_move("y", Destination::Starred).is_err());
        assert!(store.begin_sort("channels", SortMode::Recent).is_err());
    }

    #[test]
    fn moves_show_at_once_and_settle() {
        let mut store = ready();
        let (ticket, _) = store
            .begin_move("x", Destination::Section(Some("a".into())))
            .unwrap();
        let state = store.state(vec![]);
        let data = state.data.clone().unwrap();
        assert!(data.starred.is_empty(), "moving out of Starred unstars");
        assert_eq!(data.assignments.get("x").map(String::as_str), Some("a"));
        assert!(state.moves[0].pending && state.saving());
        let groups = SidebarGroups {
            sections: vec![section("a", 0.0)],
            assignments: [("x".to_string(), "a".to_string())].into(),
        };
        store
            .finish_move(ticket, Ok((Some(groups), vec![])))
            .unwrap();
        let state = store.state(vec![]);
        assert!(state.moves.is_empty());
        assert_eq!(store.confirmed().unwrap().assignments.len(), 1);
    }

    #[test]
    fn a_new_section_appears_after_the_last() {
        let mut store = ready();
        let new = NewSection {
            id: "b".into(),
            name: " Work ".into(),
        };
        store.begin_move("y", Destination::Create(new)).unwrap();
        let data = store.state(vec![]).data.unwrap();
        assert_eq!(data.sections[1].name, "Work");
        assert_eq!(data.sections[1].order, 1.0);
        assert_eq!(data.assignments.get("y").map(String::as_str), Some("b"));
    }

    #[test]
    fn failed_moves_roll_back_and_can_be_retried() {
        let mut store = ready();
        let (ticket, _) = store.begin_move("x", Destination::Section(None)).unwrap();
        assert!(store.finish_move(ticket, Err("conflict".into())).is_err());
        let state = store.state(vec![]);
        assert_eq!(state.data.unwrap().starred, vec!["x".to_string()]);
        assert_eq!(state.moves[0].error.as_deref(), Some("conflict"));
        assert!(!state.moves[0].pending);
        let (_, intent) = store.retry_move("x").unwrap();
        assert_eq!(intent.destination, Destination::Section(None));
        assert!(store.state(vec![]).moves.iter().all(|m| m.pending));
        store.dismiss_move("x");
    }

    #[test]
    fn an_older_failure_does_not_replace_a_newer_move() {
        let mut store = ready();
        let (first, _) = store.begin_move("x", Destination::Section(None)).unwrap();
        let (second, _) = store.begin_move("x", Destination::Starred).unwrap();
        let _ = store.finish_move(first, Err("old".into()));
        assert!(store.state(vec![]).moves.iter().all(|m| m.pending));
        store
            .finish_move(second, Ok((None, vec!["x".into()])))
            .unwrap();
        assert!(store.state(vec![]).moves.is_empty());
    }

    #[test]
    fn reads_wait_for_moves_and_drop_when_stale() {
        let mut store = ready();
        let (ticket, _) = store.begin_move("x", Destination::Starred).unwrap();
        assert!(store.begin_read().is_none(), "waits for the move");
        store
            .finish_move(ticket, Ok((None, vec!["x".into()])))
            .unwrap();
        let read = store.begin_read().unwrap();
        let mute = store.begin_mute().unwrap();
        store.finish_mute(mute, vec!["x".into()]).unwrap();
        assert!(!store.finish_read(read, Ok(SidebarPreferences::default())));
        assert_eq!(store.confirmed().unwrap().muted, vec!["x".to_string()]);
    }

    #[test]
    fn sorts_are_optimistic_and_keep_the_latest_failure() {
        let mut store = ready();
        let ticket = store.begin_sort("channels", SortMode::Recent).unwrap();
        let data = store.state(vec![]).data.unwrap();
        assert_eq!(data.sort_mode("channels"), SortMode::Recent);
        store.finish_sort(ticket, Err("offline".into()));
        let state = store.state(vec![]);
        assert_eq!(state.data.unwrap().sort_mode("channels"), SortMode::Alpha);
        assert_eq!(state.sort_errors[0].mode, SortMode::Recent);
        store.dismiss_sort("channels");
        assert!(store.state(vec![]).sort_errors.is_empty());
    }

    #[test]
    fn removing_a_section_applies_both_steps() {
        let mut store = ready();
        assert!(store.begin_remove_section("missing").is_err());
        let (ticket, ids) = store.begin_remove_section("a").unwrap();
        assert_eq!(ids, vec!["a".to_string()]);
        store
            .finish_remove_section(ticket, Some(BTreeMap::new()), Ok(SidebarGroups::default()))
            .unwrap();
        assert!(store.confirmed().unwrap().sections.is_empty());
    }

    #[test]
    fn clear_fences_results_in_flight() {
        let mut store = ready();
        let (ticket, _) = store.begin_move("x", Destination::Starred).unwrap();
        store.clear();
        assert!(store.finish_move(ticket, Ok((None, vec![]))).is_err());
        assert_eq!(store.state(vec![]).status, PreferencesStatus::Idle);
    }
}
