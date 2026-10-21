//! Opt-in timing of the paths that must feel fast: startup, switching
//! communities, opening threads, sending and menus.
//!
//! `BUZZ_PERF=1` logs each flow's steps to stderr, prefixed `[buzz::perf]`,
//! as milliseconds since the flow began; `BUZZ_PERF=frames` also logs the
//! CPU time of every frame (render, layout and paint, before the GPU) and
//! when it ended, after the newest flow began.
//! Off, every probe is one relaxed atomic load.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);
static FRAMES: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

/// Called first thing in `main`: the startup flow begins here.
pub fn init() {
    let start = *START.get_or_init(Instant::now);
    let value = std::env::var("BUZZ_PERF").unwrap_or_default();
    if value.is_empty() || value == "0" {
        return;
    }
    ON.store(true, Ordering::Relaxed);
    FRAMES.store(value == "frames", Ordering::Relaxed);
    STATE.with_borrow_mut(|state| state.begin_at(STARTUP, start));
}

pub const STARTUP: &str = "startup";
pub const SWITCH: &str = "switch";
pub const THREAD: &str = "thread";
pub const SEND: &str = "send";
pub const MENU: &str = "menu";

#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Starts (or restarts) `flow` now.
#[inline]
pub fn begin(flow: &'static str) {
    if on() {
        STATE.with_borrow_mut(|state| state.begin_at(flow, Instant::now()));
    }
}

/// Logs `step` of `flow` now, once per run of the flow.
#[inline]
pub fn step(flow: &'static str, step: &'static str) {
    if on() {
        STATE.with_borrow_mut(|state| state.log(flow, step, Instant::now()));
    }
}

/// Logs `step` of `flow` when the frame being built has been painted;
/// called from `render` once what the step names is on screen.
#[inline]
pub fn shown(flow: &'static str, step: &'static str) {
    if on() {
        STATE.with_borrow_mut(|state| {
            if state.open(flow, step) && !state.pending.contains(&(flow, step)) {
                state.pending.push((flow, step));
            }
        });
    }
}

/// Whether the running `flow` has logged `step`.
pub fn logged(flow: &'static str, step: &'static str) -> bool {
    on() && STATE.with_borrow(|state| {
        state
            .flows
            .iter()
            .any(|f| f.name == flow && f.logged.contains(&step))
    })
}

/// The window's root began rendering a frame.
#[inline]
pub fn frame_start() {
    if on() {
        STATE.with_borrow_mut(|state| state.frame = Some(Instant::now()));
    }
}

/// The window's root finished painting a frame.
#[inline]
pub fn frame_end() {
    if !on() {
        return;
    }
    let now = Instant::now();
    STATE.with_borrow_mut(|state| {
        state.log(STARTUP, "first_frame", now);
        for (flow, step) in std::mem::take(&mut state.pending) {
            state.log(flow, step, now);
        }
        if let Some(started) = state.frame.take()
            && FRAMES.load(Ordering::Relaxed)
        {
            let ms = now.duration_since(started).as_secs_f64() * 1000.;
            // The newest flow is the input the frame answers.
            let since = state.flows.last().map_or(0., |flow| {
                now.duration_since(flow.started).as_secs_f64() * 1000.
            });
            eprintln!("[buzz::perf] frame {ms:.2} ms (+{since:.1} ms)");
        }
    });
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

#[derive(Default)]
struct State {
    flows: Vec<Flow>,
    pending: Vec<(&'static str, &'static str)>,
    frame: Option<Instant>,
}

struct Flow {
    name: &'static str,
    started: Instant,
    logged: Vec<&'static str>,
}

impl State {
    fn begin_at(&mut self, name: &'static str, started: Instant) {
        self.flows.retain(|flow| flow.name != name);
        self.pending.retain(|(flow, _)| *flow != name);
        self.flows.push(Flow {
            name,
            started,
            logged: Vec::new(),
        });
    }

    /// Whether `flow` runs and has not logged `step` yet.
    fn open(&self, flow: &str, step: &str) -> bool {
        self.flows
            .iter()
            .any(|f| f.name == flow && !f.logged.contains(&step))
    }

    fn log(&mut self, flow: &str, step: &'static str, at: Instant) {
        let Some(f) = self.flows.iter_mut().find(|f| f.name == flow) else {
            return;
        };
        if f.logged.contains(&step) {
            return;
        }
        f.logged.push(step);
        let ms = at.duration_since(f.started).as_secs_f64() * 1000.;
        eprintln!("[buzz::perf] {flow}.{step} {ms:.1} ms");
    }
}
