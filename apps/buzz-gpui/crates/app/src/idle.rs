//! Automatic away (relay.md §5h.2, buzz-app's `presence/activity.ts`):
//! after ten minutes without input, every community publishes away until
//! input returns.
//!
//! Input is what the user does in the window: pointer presses and moves,
//! keys, the wheel, and the window coming to the front, captured before any
//! control handles them ([`Idle::input`]). Where the desktop reports
//! machine-wide idle time (GNOME's Mutter idle monitor over D-Bus, under
//! Wayland and X11; the system's own count on macOS and Windows, which
//! buzz-app reads there too) it is sampled every 30 s and counts too, so
//! typing in another app keeps the user online; elsewhere only input in
//! Buzz does, as in buzz-app on Linux.
//!
//! The window being shown is tracked here as well: while it is hidden in
//! the tray, sessions stop observing other people's presence.

use crate::community::AppState;
use futures::StreamExt;
use gpui_kit::{App, Global};
use std::time::{Duration, Instant};

const AWAY_AFTER: Duration = Duration::from_secs(600);
const SAMPLE_EVERY: Duration = Duration::from_secs(30);

pub struct Idle {
    pub idle: bool,
    /// The main window is on screen.
    pub visible: bool,
    /// The last input seen, in Buzz or (sampled) anywhere on the machine.
    last: Instant,
}

impl Global for Idle {}

/// Whether input last seen at `last` makes the user away at `now`.
fn away(last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last) >= AWAY_AFTER
}

impl Idle {
    pub fn init(cx: &mut App) {
        cx.set_global(Self {
            idle: false,
            visible: true,
            last: Instant::now(),
        });
        Self::arm(cx);
        let (sender, mut samples) = futures::channel::mpsc::unbounded();
        let started = std::thread::Builder::new()
            .name("buzz-idle".into())
            .spawn(move || {
                let Some(monitor) = Monitor::connect() else {
                    tracing::info!("no machine idle monitor; following input in Buzz");
                    return;
                };
                loop {
                    let Some(idle) = monitor.idle_time() else {
                        tracing::info!("machine idle monitor stopped answering");
                        return;
                    };
                    if sender.unbounded_send((Instant::now(), idle)).is_err() {
                        return;
                    }
                    std::thread::sleep(SAMPLE_EVERY);
                }
            });
        if let Err(error) = started {
            tracing::warn!(%error, "could not start the idle monitor");
            return;
        }
        cx.spawn(async move |cx| {
            while let Some((at, idle)) = samples.next().await {
                cx.update(|cx| {
                    if let Some(input) = at.checked_sub(idle) {
                        Self::seen(input, cx);
                    }
                    Self::update(cx);
                });
            }
        })
        .detach();
    }

    /// Input in the window, or the window coming to the front.
    pub fn input(cx: &mut App) {
        if cx.has_global::<Self>() {
            Self::seen(Instant::now(), cx);
        }
    }

    fn seen(at: Instant, cx: &mut App) {
        let this = cx.global_mut::<Self>();
        if at <= this.last {
            return;
        }
        this.last = at;
        // Back from away at once; otherwise the running timer re-arms
        // itself from `last`.
        if this.idle {
            Self::update(cx);
        }
    }

    /// Re-evaluates away now.
    fn update(cx: &mut App) {
        let now = Instant::now();
        let this = cx.global::<Self>();
        let idle = away(this.last, now);
        if this.idle != idle {
            cx.global_mut::<Self>().idle = idle;
            let communities = cx.global::<AppState>().communities.clone();
            for community in communities {
                community.read(cx).session.set_idle(idle);
            }
        }
    }

    /// One timer, re-armed for `last + AWAY_AFTER` until away.
    fn arm(cx: &mut App) {
        cx.spawn(async move |cx| {
            loop {
                let wait = cx.update(|cx| {
                    let this = cx.global::<Self>();
                    (this.last + AWAY_AFTER)
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(250))
                });
                cx.background_executor().timer(wait.min(SAMPLE_EVERY)).await;
                cx.update(Self::update);
            }
        })
        .detach();
    }

    /// The main window was shown or hidden.
    pub fn set_visible(visible: bool, cx: &mut App) {
        if !cx.has_global::<Self>() || cx.global::<Self>().visible == visible {
            return;
        }
        cx.global_mut::<Self>().visible = visible;
        if visible {
            Self::input(cx);
        }
        let communities = cx.global::<AppState>().communities.clone();
        for community in communities {
            community.read(cx).session.set_visible(visible);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
struct Monitor(zbus::blocking::Connection);

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl Monitor {
    fn connect() -> Option<Self> {
        let monitor = Self(zbus::blocking::Connection::session().ok()?);
        monitor.idle_time().map(|_| monitor)
    }

    fn idle_time(&self) -> Option<Duration> {
        let reply = self
            .0
            .call_method(
                Some("org.gnome.Mutter.IdleMonitor"),
                "/org/gnome/Mutter/IdleMonitor/Core",
                Some("org.gnome.Mutter.IdleMonitor"),
                "GetIdletime",
                &(),
            )
            .ok()?;
        let millis: u64 = reply.body().deserialize().ok()?;
        Some(Duration::from_millis(millis))
    }
}

#[cfg(any(target_os = "macos", windows))]
struct Monitor;

#[cfg(any(target_os = "macos", windows))]
impl Monitor {
    fn connect() -> Option<Self> {
        Self.idle_time().map(|_| Self)
    }

    fn idle_time(&self) -> Option<Duration> {
        user_idle::UserIdle::get_time()
            .ok()
            .map(|idle| idle.duration())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn away_after_ten_minutes_without_input() {
        let start = Instant::now();
        assert!(!away(start, start + Duration::from_secs(599)));
        assert!(away(start, start + AWAY_AFTER));
        // A later input (in Buzz or elsewhere) starts the wait over.
        let later = start + Duration::from_secs(300);
        assert!(!away(later, start + AWAY_AFTER));
    }
}
