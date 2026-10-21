//! The card's pairing client, ported from buzz-app's
//! `src/features/pairing/client.ts`: it starts attempts with fresh ids,
//! polls the native session every 400ms, holds its own "transferring" and
//! "cancelling" display while a decision or cancel is in flight, and keeps
//! a failed cleanup visible so it blocks the next attempt instead of
//! abandoning a live session.

use crate::community::{AppState, IdentityState};
use crate::runtime::Runtime;
use buzz_pair_mobile::{Pairing, Status};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use gpui_kit::{App, Context, Global, Task};
use std::rc::Rc;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(400);
const CHECK_FAILED: &str = "Couldn’t check pairing. Try again.";
const CANCEL_FAILED: &str = "Couldn’t cancel pairing. Close this window before trying again.";

/// What the client asks of the native session (`PairingNative`).
pub(super) trait Native: 'static {
    /// Pairing needs the app's own identity (`pairingAvailable`).
    fn available(&self) -> bool {
        true
    }
    /// The account pairing would send.
    fn account(&self, cx: &App) -> Result<String, String>;
    fn start(&self, id: &str, viewer: &str, community: &str, cx: &App) -> Result<(), String>;
    fn status(&self, id: &str) -> Result<Status, String>;
    fn confirm(&self, id: &str) -> Result<(), String>;
    fn deny(&self, id: &str) -> Result<(), String>;
    /// Runs to completion even if the returned future is dropped.
    fn cancel(&self, id: &str, cx: &App) -> BoxFuture<'static, Result<Status, String>>;
}

/// The app's one pairing session owner, which sign-out closes.
#[derive(Default)]
struct Owner(Pairing);

impl Global for Owner {}

/// Signing out: stop any attempt before it can send the key, and refuse
/// new ones until Buzz restarts.
pub fn close(cx: &mut App) {
    if let Some(owner) = cx.try_global::<Owner>() {
        owner.0.close();
    }
}

/// The native session on this app's identity and tokio runtime.
pub(super) struct AppNative(Pairing);

impl AppNative {
    pub(super) fn new(cx: &mut App) -> Self {
        Self(cx.default_global::<Owner>().0.clone())
    }
}

impl Native for AppNative {
    fn account(&self, cx: &App) -> Result<String, String> {
        account(cx)
    }

    fn start(&self, id: &str, viewer: &str, community: &str, cx: &App) -> Result<(), String> {
        self.0
            .start(&Runtime::handle(cx), identity(cx), id, viewer, community)
    }

    fn status(&self, id: &str) -> Result<Status, String> {
        self.0.status(id)
    }

    fn confirm(&self, id: &str) -> Result<(), String> {
        self.0.confirm(id)
    }

    fn deny(&self, id: &str) -> Result<(), String> {
        self.0.deny(id)
    }

    fn cancel(&self, id: &str, cx: &App) -> BoxFuture<'static, Result<Status, String>> {
        let pairing = self.0.clone();
        let id = id.to_owned();
        let task = Runtime::handle(cx).spawn(async move { pairing.cancel(&id).await });
        async move { task.await.unwrap_or_else(|_| Err(CANCEL_FAILED.into())) }.boxed()
    }
}

/// The public key of the app's identity (`pairing_account`).
pub(super) fn account(cx: &App) -> Result<String, String> {
    buzz_pair_mobile::account(identity(cx).as_deref())
}

fn identity(cx: &App) -> Option<std::sync::Arc<buzz_identity::Identity>> {
    match &cx.global::<AppState>().identity {
        IdentityState::Ready(identity) => Some(identity.clone()),
        _ => None,
    }
}

type Cleanup = Shared<BoxFuture<'static, Result<Option<Status>, String>>>;

struct Session {
    id: String,
    deciding: bool,
    /// The next status read; dropping it is `clearTimeout`.
    poll: Option<Task<()>>,
}

pub(super) struct PairingClient {
    native: Rc<dyn Native>,
    state: Status,
    generation: u64,
    active: Option<Session>,
    /// The last attempt's cancellation. A failure stays here and fails the
    /// next start.
    cleanup: Cleanup,
}

impl PairingClient {
    pub(super) fn new(native: Rc<dyn Native>, cx: &mut Context<Self>) -> Self {
        // Leaving the section disposes the attempt, even mid-start.
        cx.on_release(|this: &mut Self, cx| {
            if let Some(session) = this.active.take() {
                drop(this.native.cancel(&session.id, cx));
            }
        })
        .detach();
        Self {
            native,
            state: Status::Idle,
            generation: 0,
            active: None,
            cleanup: futures::future::ready(Ok(None)).boxed().shared(),
        }
    }

    pub(super) fn state(&self) -> &Status {
        &self.state
    }

    pub(super) fn native(&self) -> Rc<dyn Native> {
        self.native.clone()
    }

    fn update(&mut self, next: Status, cx: &mut Context<Self>) {
        self.state = next;
        cx.notify();
    }

    fn is_active(&self, id: &str) -> bool {
        self.active.as_ref().is_some_and(|session| session.id == id)
    }

    fn stop(&mut self, cx: &mut Context<Self>) -> Cleanup {
        if let Some(previous) = self.active.take() {
            drop(previous.poll);
            self.cleanup = self
                .native
                .cancel(&previous.id, cx)
                .map(|outcome| outcome.map(Some))
                .boxed()
                .shared();
        }
        self.cleanup.clone()
    }

    pub(super) fn start(&mut self, viewer: String, community: String, cx: &mut Context<Self>) {
        self.generation += 1;
        let attempt = self.generation;
        self.update(Status::Connecting, cx);
        let cleanup = self.stop(cx);
        cx.spawn(async move |this, cx| {
            let stopped = cleanup.await;
            let _ = this.update(cx, |this, cx| {
                if let Err(message) = stopped {
                    if attempt == this.generation {
                        this.update(Status::Error { message }, cx);
                    }
                    return;
                }
                if attempt != this.generation {
                    return;
                }
                let id = uuid::Uuid::new_v4().to_string();
                this.active = Some(Session {
                    id: id.clone(),
                    deciding: false,
                    poll: None,
                });
                match this.native.start(&id, &viewer, &community, cx) {
                    Ok(()) => this.poll(id, attempt, cx),
                    Err(message) => {
                        if attempt == this.generation {
                            this.update(Status::Error { message }, cx);
                        }
                    }
                }
            });
        })
        .detach();
    }

    fn poll(&mut self, id: String, attempt: u64, cx: &mut Context<Self>) {
        match self.native.status(&id) {
            Ok(status) => {
                if !self.is_active(&id) || attempt != self.generation {
                    return;
                }
                let deciding = self.active.as_ref().is_some_and(|s| s.deciding);
                if replaces(&self.state, deciding, &status) {
                    self.update(status.clone(), cx);
                }
                if running(&status) {
                    let next = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(POLL).await;
                        let _ = this.update(cx, |this, cx| this.poll(id, attempt, cx));
                    });
                    if let Some(session) = self.active.as_mut() {
                        session.poll = Some(next);
                    }
                }
            }
            Err(_) => {
                if !self.is_active(&id) {
                    return;
                }
                let cancelled = self.generation + 1;
                let done = self.cancel(false, cx);
                cx.spawn(async move |this, cx| {
                    done.await;
                    let _ = this.update(cx, |this, cx| {
                        if this.generation == cancelled && this.state == Status::Cancelled {
                            this.update(
                                Status::Error {
                                    message: CHECK_FAILED.into(),
                                },
                                cx,
                            );
                        }
                    });
                })
                .detach();
            }
        }
    }

    /// "Codes match" or Cancel for a phone that compares codes.
    pub(super) fn decide(&mut self, confirm: bool, cx: &mut Context<Self>) {
        let Some(session) = self.active.as_mut() else {
            return;
        };
        if !decidable(&self.state) || session.deciding {
            return;
        }
        session.deciding = true;
        let id = session.id.clone();
        // Do not invent a new pairing operation while an abort is pending.
        self.update(
            if confirm {
                Status::Transferring
            } else {
                Status::Cancelling
            },
            cx,
        );
        let decided = if confirm {
            self.native.confirm(&id)
        } else {
            self.native.deny(&id)
        };
        if let Err(message) = decided
            && self.is_active(&id)
        {
            self.update(Status::Error { message }, cx);
        }
        if let Some(session) = self.active.as_mut().filter(|s| s.id == id) {
            session.deciding = false;
        }
    }

    /// Stops the attempt. `reset` (leaving, or another account or
    /// community) returns to idle; otherwise the user sees the outcome,
    /// which native reports as uncertain if the account may have been sent.
    pub(super) fn cancel(&mut self, reset: bool, cx: &mut Context<Self>) -> Task<()> {
        self.generation += 1;
        let attempt = self.generation;
        self.update(Status::Cancelling, cx);
        let stopping = self.active.is_some();
        let cleanup = self.stop(cx);
        cx.spawn(async move |this, cx| {
            let outcome = cleanup.await;
            let _ = this.update(cx, |this, cx| {
                if attempt == this.generation {
                    this.update(cancelled(outcome, reset, stopping), cx);
                }
            });
        })
    }
}

/// Whether a polled status replaces what the card shows. A decision or
/// cancel in flight keeps its own display over a code or transfer that
/// native reported before it.
fn replaces(shown: &Status, deciding: bool, polled: &Status) -> bool {
    let held = deciding || matches!(shown, Status::Transferring | Status::Cancelling);
    !(held && matches!(polled, Status::Code { .. } | Status::Transferring))
}

/// Native is still working, so the card keeps polling.
fn running(status: &Status) -> bool {
    matches!(
        status,
        Status::Connecting | Status::Qr { .. } | Status::Code { .. } | Status::Transferring
    )
}

/// Only an older phone's comparison waits for the desktop.
fn decidable(shown: &Status) -> bool {
    matches!(
        shown,
        Status::Code {
            code_entry: false,
            ..
        }
    )
}

/// What a finished cancel shows. `stopping` says a live attempt was
/// stopped; without one, an earlier cleanup's result is not this outcome.
fn cancelled(outcome: Result<Option<Status>, String>, reset: bool, stopping: bool) -> Status {
    match outcome {
        Err(_) => Status::Error {
            message: CANCEL_FAILED.into(),
        },
        Ok(Some(outcome)) if !reset && stopping => outcome,
        Ok(_) if reset => Status::Idle,
        Ok(_) => Status::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn code(code_entry: bool) -> Status {
        Status::Code {
            code: "001234".into(),
            code_entry,
        }
    }

    #[test]
    fn a_decision_in_flight_is_not_revived_by_a_stale_poll() {
        // "Codes match" shows transferring before native reports it.
        assert!(!replaces(&Status::Transferring, false, &code(false)));
        assert!(!replaces(
            &Status::Transferring,
            false,
            &Status::Transferring
        ));
        // A denial keeps "cancelling" while native still shows the code.
        assert!(!replaces(&Status::Cancelling, false, &code(false)));
        assert!(!replaces(&code(false), true, &code(false)));
        // Its outcome still lands.
        let denied = Status::Error {
            message: "Pairing was canceled.".into(),
        };
        assert!(replaces(&Status::Cancelling, false, &denied));
        assert!(replaces(&Status::Transferring, false, &Status::Complete));
        assert!(replaces(&Status::Connecting, false, &code(true)));
    }

    #[test]
    fn polling_stops_at_an_outcome() {
        for live in [Status::Connecting, code(true), Status::Transferring] {
            assert!(running(&live), "{live:?}");
        }
        for done in [
            Status::Complete,
            Status::Expired,
            Status::Uncertain,
            Status::Cancelled,
            Status::Cancelling,
            Status::Error {
                message: String::new(),
            },
        ] {
            assert!(!running(&done), "{done:?}");
        }
    }

    #[test]
    fn only_a_legacy_comparison_takes_a_desktop_decision() {
        assert!(decidable(&code(false)));
        assert!(!decidable(&code(true)));
        assert!(!decidable(&Status::Transferring));
    }

    #[test]
    fn cancel_shows_what_native_knows() {
        // A transfer may have reached the phone.
        assert_eq!(
            cancelled(Ok(Some(Status::Uncertain)), false, true),
            Status::Uncertain
        );
        // Leaving resets, whatever native says.
        assert_eq!(
            cancelled(Ok(Some(Status::Uncertain)), true, true),
            Status::Idle
        );
        // Nothing live: an earlier cleanup isn't this outcome.
        assert_eq!(
            cancelled(Ok(Some(Status::Uncertain)), false, false),
            Status::Cancelled
        );
        assert_eq!(cancelled(Ok(None), false, true), Status::Cancelled);
        // A failed cleanup stays visible, even when resetting.
        for reset in [false, true] {
            assert_eq!(
                cancelled(Err("timeout".into()), reset, true),
                Status::Error {
                    message: CANCEL_FAILED.into()
                }
            );
        }
    }
}
