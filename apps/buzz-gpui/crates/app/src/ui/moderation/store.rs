//! One verified community roster shared by Settings navigation, the
//! Membership card and the rail menu (`createCommunityMembership`).
//!
//! The roster belongs to the selected community's current connection: a
//! new community or a reconnect starts over, so another community's roles
//! never gate this one's controls. Reads run only while something holds a
//! demand ([`ensure`]); a newer read always wins over an older one.

use crate::community::AppState;
use crate::ui::settings::cards;
use buzz_relay_client::SessionStatus;
use buzz_relay_client::moderation::{Member, Role, manages};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Loading,
    Ready,
    Error,
}

/// What the roster shows now (`MembershipSnapshot`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub status: Status,
    /// `None` before a read, or when the community publishes no list.
    pub members: Option<Arc<[Member]>>,
    pub role: Option<Role>,
    pub refreshing: bool,
    pub error: Option<String>,
}

impl Snapshot {
    const INITIAL: Self = Self {
        status: Status::Idle,
        members: None,
        role: None,
        refreshing: false,
        error: None,
    };

    /// The viewer owns or administers the community (`canManageMembership`).
    pub fn can_manage(&self) -> bool {
        self.status == Status::Ready && manages(self.role)
    }
}

/// The connection a roster belongs to: the community, and whether its
/// session is ready (`generation:scope`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub community: EntityId,
    pub ready: bool,
}

/// The roster's state machine, without I/O.
#[derive(Debug)]
pub struct Roster {
    key: Option<Key>,
    value: Snapshot,
    /// The current read; any other read's answer is dropped.
    generation: u64,
}

impl Default for Roster {
    fn default() -> Self {
        Self {
            key: None,
            value: Snapshot::INITIAL,
            generation: 0,
        }
    }
}

impl Roster {
    /// The snapshot for `key`; another connection's is never shown.
    pub fn snapshot(&self, key: Option<Key>) -> Snapshot {
        if key.is_some() && self.key == key {
            self.value.clone()
        } else {
            Snapshot::INITIAL
        }
    }

    /// Follows the connection; a new one starts over and cancels the read.
    pub fn rekey(&mut self, key: Option<Key>) -> bool {
        if key == self.key {
            return false;
        }
        self.key = key;
        self.generation += 1;
        self.value = Snapshot::INITIAL;
        true
    }

    /// Starts a read unless one is running or the list is already known.
    /// `fresh` always re-reads; `reacquired` re-reads a known list for a
    /// new demand. Returns the read's token.
    pub fn begin(&mut self, fresh: bool, reacquired: bool) -> Option<u64> {
        if !self.key.is_some_and(|key| key.ready) {
            return None;
        }
        let status = self.value.status;
        if !fresh && (status == Status::Loading || (status == Status::Ready && !reacquired)) {
            return None;
        }
        self.generation += 1;
        let retained = (status == Status::Ready)
            .then(|| self.value.members.clone())
            .flatten();
        self.value = Snapshot {
            status: if retained.is_some() {
                Status::Ready
            } else {
                Status::Loading
            },
            members: retained,
            role: self.value.role,
            refreshing: true,
            error: None,
        };
        Some(self.generation)
    }

    /// Takes a read's answer. A failure keeps a list already shown and
    /// marks it out of date. `false` when the read was superseded.
    pub fn finish(
        &mut self,
        token: u64,
        viewer: &str,
        result: Result<Option<Vec<Member>>, String>,
    ) -> bool {
        if token != self.generation {
            return false;
        }
        self.value = match result {
            Ok(members) => Snapshot {
                status: Status::Ready,
                role: members.as_ref().and_then(|members| {
                    members
                        .iter()
                        .find(|member| member.pubkey == viewer)
                        .map(|member| member.role)
                }),
                members: members.map(Arc::from),
                refreshing: false,
                error: None,
            },
            Err(error) => {
                let retained = self.value.members.clone();
                Snapshot {
                    status: if retained.is_some() {
                        Status::Ready
                    } else {
                        Status::Error
                    },
                    members: retained,
                    role: self.value.role,
                    refreshing: false,
                    error: Some(format!("Could not load members: {error}")),
                }
            }
        };
        true
    }
}

type Read = Shared<BoxFuture<'static, ()>>;

/// The app's one roster.
struct Store {
    roster: Roster,
    /// Holders of [`ensure`]; reads wait for one.
    demands: Rc<Cell<usize>>,
    /// The running read; awaiting it waits for its answer.
    read: Option<(u64, Read)>,
    _community: Option<Subscription>,
}

impl Global for Store {}

pub fn init(cx: &mut App) {
    cx.set_global(Store {
        roster: Roster::default(),
        demands: Rc::new(Cell::new(0)),
        read: None,
        _community: None,
    });
    cx.observe_global::<AppState>(follow).detach();
    follow(cx);
}

/// Follows the selected community; each one notifies when its session
/// changes.
fn follow(cx: &mut App) {
    let community = AppState::current(cx);
    let observer = community.map(|community| cx.observe(&community, |_, cx| relay_changed(cx)));
    cx.global_mut::<Store>()._community = observer;
    relay_changed(cx);
}

/// The selected community's connection now.
pub fn key(cx: &App) -> Option<Key> {
    let community = AppState::current(cx)?;
    let ready = community.read(cx).snapshot.status == SessionStatus::Ready;
    Some(Key {
        community: community.entity_id(),
        ready,
    })
}

/// The roster for the selected community.
pub fn snapshot(cx: &App) -> Snapshot {
    cx.try_global::<Store>()
        .map_or(Snapshot::INITIAL, |store| store.roster.snapshot(key(cx)))
}

fn relay_changed(cx: &mut App) {
    let key = key(cx);
    if cx.global_mut::<Store>().roster.rekey(key) {
        cx.global_mut::<Store>().read = None;
        publish(cx);
        if cx.global::<Store>().demands.get() > 0 {
            load(false, false, cx);
        }
    }
}

fn publish(cx: &mut App) {
    cx.update_global::<Store, _>(|_, _| {});
    cards::changed(cx);
}

/// Reads the roster if needed and returns when the read in flight, if
/// any, has answered.
fn load(fresh: bool, reacquired: bool, cx: &mut App) -> Option<Read> {
    let key = key(cx);
    if cx.global_mut::<Store>().roster.rekey(key) {
        cx.global_mut::<Store>().read = None;
    }
    let community = AppState::current(cx)?;
    let Some(token) = cx.global_mut::<Store>().roster.begin(fresh, reacquired) else {
        return cx
            .global::<Store>()
            .read
            .as_ref()
            .map(|(_, read)| read.clone());
    };
    publish(cx);
    let session = community.read(cx).session.clone();
    let viewer = session.viewer().to_owned();
    let (done, finished) = futures::channel::oneshot::channel::<()>();
    let read: Read = finished.map(|_| ()).boxed().shared();
    cx.global_mut::<Store>().read = Some((token, read.clone()));
    cx.spawn(async move |cx| {
        let result = if dev_fail() {
            Err("relay unavailable".to_owned())
        } else {
            session.community_roster().await
        };
        if let Err(error) = &result {
            tracing::warn!(%error, "could not read the community roster");
        }
        cx.update(|cx| {
            let store = cx.global_mut::<Store>();
            if store.read.as_ref().is_some_and(|(t, _)| *t == token) {
                store.read = None;
            }
            if store.roster.finish(token, &viewer, result) {
                publish(cx);
            }
        });
        let _ = done.send(());
    })
    .detach();
    Some(read)
}

/// Development only: the roster reads numbered in `BUZZ_DEV_ROSTER_FAIL`
/// (comma separated, from 1) fail, so headless runs can show the
/// out-of-date guard.
fn dev_fail() -> bool {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static READS: AtomicUsize = AtomicUsize::new(0);
    let Ok(list) = std::env::var("BUZZ_DEV_ROSTER_FAIL") else {
        return false;
    };
    let read = READS.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::debug!(read, "development roster read");
    list.split(',').any(|item| item.trim().parse() == Ok(read))
}

/// Holds the roster while the subscription lives; the first demand after
/// none re-reads, so a role changed elsewhere shows.
pub fn ensure(cx: &mut App) -> Subscription {
    let demands = cx.global::<Store>().demands.clone();
    let reacquired = demands.get() == 0;
    demands.set(demands.get() + 1);
    load(false, reacquired, cx);
    Subscription::new(move || demands.set(demands.get().saturating_sub(1)))
}

/// Re-reads the roster; the future ends when the read has answered.
pub fn refresh(cx: &mut App) -> impl std::future::Future<Output = ()> + use<> {
    let read = load(true, false, cx);
    async move {
        if let Some(read) = read {
            read.await;
        }
    }
}

/// Re-reads the roster without waiting for it.
pub fn reload(cx: &mut App) {
    load(true, false, cx);
}

/// Calls `f` whenever the roster changes.
pub fn observe<T: 'static>(
    cx: &mut Context<T>,
    f: impl FnMut(&mut T, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global::<Store>(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    const VIEWER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn key(ready: bool) -> Option<Key> {
        Some(Key {
            community: EntityId::from(1u64),
            ready,
        })
    }

    fn list(role: Role) -> Result<Option<Vec<Member>>, String> {
        Ok(Some(vec![Member {
            pubkey: VIEWER.into(),
            role,
        }]))
    }

    #[test]
    fn projects_navigation_authorization_from_the_verified_roster() {
        for (role, allowed) in [
            (Role::Owner, true),
            (Role::Admin, true),
            (Role::Member, false),
        ] {
            let mut roster = Roster::default();
            roster.rekey(key(true));
            let token = roster.begin(false, true).unwrap();
            assert_eq!(roster.snapshot(key(true)).status, Status::Loading);
            assert!(roster.snapshot(key(true)).refreshing);
            assert!(roster.finish(token, VIEWER, list(role)));
            assert_eq!(roster.snapshot(key(true)).can_manage(), allowed);
        }
    }

    #[test]
    fn refreshes_after_demand_is_released_and_reacquired() {
        let mut roster = Roster::default();
        roster.rekey(key(true));
        let token = roster.begin(false, true).unwrap();
        roster.finish(token, VIEWER, list(Role::Member));
        // A known list is not re-read for the same demand...
        assert_eq!(roster.begin(false, false), None);
        // ...but is for a new one, and then shows the new role.
        let token = roster.begin(false, true).unwrap();
        roster.finish(token, VIEWER, list(Role::Admin));
        assert!(roster.snapshot(key(true)).can_manage());
    }

    #[test]
    fn waits_for_a_ready_connection_and_never_shows_another() {
        let mut roster = Roster::default();
        roster.rekey(key(false));
        assert_eq!(roster.begin(true, true), None);
        roster.rekey(key(true));
        let token = roster.begin(false, true).unwrap();
        roster.finish(token, VIEWER, list(Role::Owner));
        let other = Some(Key {
            community: EntityId::from(2u64),
            ready: true,
        });
        assert_eq!(roster.snapshot(other).status, Status::Idle);
        assert_eq!(roster.snapshot(None).status, Status::Idle);
        // A reconnect starts over and drops the read in flight.
        let token = roster.begin(true, false).unwrap();
        assert!(roster.rekey(key(false)));
        assert!(!roster.finish(token, VIEWER, list(Role::Member)));
        assert_eq!(roster.snapshot(key(false)), Snapshot::INITIAL);
    }

    #[test]
    fn a_failed_refresh_keeps_the_list_and_marks_it_out_of_date() {
        let mut roster = Roster::default();
        roster.rekey(key(true));
        // A first failure has nothing to keep.
        let token = roster.begin(false, true).unwrap();
        roster.finish(token, VIEWER, Err("offline".into()));
        let failed = roster.snapshot(key(true));
        assert_eq!(failed.status, Status::Error);
        assert_eq!(
            failed.error.as_deref(),
            Some("Could not load members: offline")
        );
        // Retry recovers.
        let token = roster.begin(true, false).unwrap();
        assert!(roster.snapshot(key(true)).error.is_none());
        roster.finish(token, VIEWER, list(Role::Owner));
        // A later failure keeps the list, the role and the status.
        let token = roster.begin(true, false).unwrap();
        let refreshing = roster.snapshot(key(true));
        assert_eq!(refreshing.status, Status::Ready);
        assert!(refreshing.refreshing && refreshing.members.is_some());
        roster.finish(token, VIEWER, Err("relay unavailable".into()));
        let stale = roster.snapshot(key(true));
        assert_eq!(stale.status, Status::Ready);
        assert_eq!(stale.role, Some(Role::Owner));
        assert!(stale.members.is_some() && stale.error.is_some());
    }

    #[test]
    fn the_newest_read_wins_and_no_list_has_no_role() {
        let mut roster = Roster::default();
        roster.rekey(key(true));
        let first = roster.begin(false, true).unwrap();
        let second = roster.begin(true, false).unwrap();
        assert!(!roster.finish(first, VIEWER, list(Role::Owner)));
        assert!(roster.finish(second, VIEWER, Ok(None)));
        let snapshot = roster.snapshot(key(true));
        assert_eq!(snapshot.status, Status::Ready);
        assert_eq!((snapshot.members, snapshot.role), (None, None));
        assert!(!roster.snapshot(key(true)).can_manage());
    }
}
