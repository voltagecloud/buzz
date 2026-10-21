//! The main window's life: opening it, and opening it again after it was
//! hidden to the tray; what minimize and close do; where it was last
//! (`buzz_desktop::window_state`); the launch surface; and the tray icon, which can
//! outlive the window.
//!
//! The app quits explicitly. Closing the window quits unless the user
//! chose close-to-tray and a panel is showing the icon, so the app is never
//! left running with no way back to it. Quitting first stops this
//! computer's agents, and does not quit if that cannot be confirmed.

use crate::community::{AppState, Community};
use crate::idle::Idle;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use crate::runtime::Runtime;
use crate::ui::launch::Launch;
use crate::ui::navigation;
use crate::ui::workspace::Workspace;
use buzz_relay_client::{ChannelType, SessionSnapshot};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::tray::{Tray, TrayEvent};

/// View-state key for the tray choices (a device preference).
const PREFERENCES: &str = "tray";
/// Device preference holding where the window was, as
/// tauri-plugin-window-state writes its file.
const GEOMETRY: &str = "window-state";
/// The window's label in that file.
const LABEL: &str = "main";
/// The header every control area sits in (ui.md §2.1).
const HEADER_HEIGHT: Pixels = px(48.);
/// Geometry is saved once moving or resizing pauses this long.
const SAVE_GEOMETRY_AFTER: Duration = Duration::from_millis(500);

/// The tray choices, for this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub show_icon: bool,
    /// The window's minimize button hides it to the tray.
    pub minimize_to_tray: bool,
    /// Closing the window keeps the app running in the tray.
    pub close_to_tray: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            show_icon: true,
            minimize_to_tray: false,
            close_to_tray: false,
        }
    }
}

actions!(
    buzz_app,
    [
        /// ⌘Q or Ctrl+Q, and Quit Buzz in the macOS app menu.
        Quit
    ]
);

/// Quits from the keyboard anywhere in the app: ⌘Q on macOS, Ctrl+Q
/// elsewhere. Quitting stops this computer's agents first, as the tray's
/// Quit does.
fn quit_shortcut(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| quit(cx));
    cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
}

/// macOS apps quit from their menu, which the menu bar always shows.
#[cfg(target_os = "macos")]
fn app_menu(cx: &mut App) {
    cx.set_menus([Menu::new("Buzz").items([MenuItem::action("Quit Buzz", Quit)])]);
}

const DEFAULT_SIZE: Size<Pixels> = Size {
    width: px(1200.),
    height: px(800.),
};
/// The smallest the window may be. Geometry below it is never saved or
/// restored: a compositor can report a 1x1 window while it maps or hides
/// it, and restoring that leaves a window no one can see.
const MIN_SIZE: Size<Pixels> = Size {
    width: px(480.),
    height: px(400.),
};

/// Whether `bounds` is a size the window can usefully have.
fn usable(bounds: &WindowBounds) -> bool {
    let size = bounds.get_bounds().size;
    size.width >= MIN_SIZE.width && size.height >= MIN_SIZE.height
}

struct MainWindow {
    handle: Option<AnyWindowHandle>,
    /// Where the window was when it was last hidden or moved, saved for
    /// the next launch.
    bounds: Option<WindowBounds>,
    /// The launch surface has shown; reopening from the tray skips it.
    launched: bool,
    /// Bumped by every move or resize, so only the last one saves.
    moves: u64,
    preferences: Preferences,
    /// A panel is showing the tray icon.
    shown: bool,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    tray: Option<Arc<Tray>>,
    /// On the main thread, where macOS and Windows keep it.
    #[cfg(any(target_os = "macos", windows))]
    tray: Option<Tray>,
    /// Mentions and direct messages unread, last told to the tray.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    attention: u32,
    _communities: Vec<Subscription>,
}

impl Global for MainWindow {}

/// Loads the tray choices and puts the icon up if wanted. Call before
/// [`open`].
pub fn init(cx: &mut App) {
    cx.set_quit_mode(QuitMode::Explicit);
    quit_shortcut(cx);
    #[cfg(target_os = "macos")]
    app_menu(cx);
    let store = cx.global::<AppState>().store.clone();
    let preferences = store
        .device_pref(PREFERENCES)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let bounds = store
        .device_pref(GEOMETRY)
        .ok()
        .flatten()
        .and_then(|json| restore_geometry(json.as_bytes(), cx));
    cx.set_global(MainWindow {
        handle: None,
        bounds,
        launched: false,
        moves: 0,
        preferences,
        shown: false,
        tray: None,
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        attention: 0,
        _communities: Vec::new(),
    });
    cx.observe_global::<AppState>(watch_communities).detach();
    watch_communities(cx);
    apply_preferences(cx);
}

/// Whether the main window is open.
pub fn is_open(cx: &App) -> bool {
    cx.try_global::<MainWindow>()
        .and_then(|main| main.handle)
        .is_some_and(|handle| cx.windows().contains(&handle))
}

/// Opens the main window, where it was last.
pub fn open(cx: &mut App) {
    if is_open(cx) {
        return;
    }
    let bounds = cx
        .global::<MainWindow>()
        .bounds
        .unwrap_or_else(|| WindowBounds::Windowed(Bounds::centered(None, DEFAULT_SIZE, cx)));
    let launch = (!cx.global::<MainWindow>().launched).then(|| Launch::new(Instant::now()));
    cx.global_mut::<MainWindow>().launched = true;
    let opened = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            window_min_size: Some(MIN_SIZE),
            app_id: Some("buzz-gpui".into()),
            // For the taskbar, Alt+Tab and the window list; the header
            // draws no title. On macOS and Windows the header takes the
            // system title bar's place, with macOS's buttons over it.
            titlebar: Some(TitlebarOptions {
                title: Some("Buzz".into()),
                appears_transparent: cfg!(any(target_os = "macos", windows)),
                traffic_light_position: cfg!(target_os = "macos")
                    .then_some(crate::ui::window_controls::MAC_BUTTONS),
            }),
            // The header is the title bar, with buzz-app's own controls.
            window_decorations: Some(WindowDecorations::Client),
            ..Default::default()
        },
        cx,
        |window, cx| {
            // The compositor's close (a shortcut, the overview) goes the
            // way of the close button.
            window.on_window_should_close(cx, |window, cx| {
                close(window, cx);
                false
            });
            let workspace = cx.new(|cx| {
                // Becoming visible again rechecks the relay (`visibilitychange`).
                cx.observe_window_activation(window, |_, window, cx| {
                    if window.is_window_active() {
                        for community in cx.global::<AppState>().communities.clone() {
                            community.read(cx).session.visible();
                        }
                    }
                })
                .detach();
                Workspace::new(window, cx)
            });
            cx.new(|cx| Root::new(workspace, launch, window, cx))
        },
    );
    match opened {
        Ok((handle, root)) => {
            cx.global_mut::<MainWindow>().handle = Some(handle);
            let workspace = root.read(cx).workspace.clone();
            navigation::install(handle, &workspace, cx);
            Idle::set_visible(true, cx);
        }
        Err(error) => {
            tracing::error!(%error, "could not open the window");
            cx.quit();
        }
    }
}

/// The minimize button: hides to the tray if chosen, else minimizes.
pub fn minimize(window: &mut Window, cx: &mut App) {
    let main = cx.global::<MainWindow>();
    if main.preferences.show_icon && main.preferences.minimize_to_tray && main.shown {
        hide(window, cx);
    } else {
        window.minimize_window();
    }
}

/// The close button, or the compositor's close: hides to the tray if
/// chosen, else quits.
pub fn close(window: &mut Window, cx: &mut App) {
    let main = cx.global::<MainWindow>();
    if main.preferences.show_icon && main.preferences.close_to_tray && main.shown {
        hide(window, cx);
    } else {
        remember_bounds(window.window_bounds(), cx);
        quit(cx);
    }
}

/// Quits once this computer's agents have stopped; if that cannot be
/// confirmed the app keeps running, as buzz-app refuses the exit. They
/// can take up to 30 seconds to stop, so that waits off the UI thread.
pub fn quit(cx: &mut App) {
    stop_agents_and_quit("Buzz couldn’t quit", || Ok(()), cx);
}

/// Quits as [`quit`] does, then starts `program`: the update just
/// installed. The new copy waits for this process to exit, so it does
/// not hand its launch to this one over the single-instance socket.
pub fn restart(program: String, cx: &mut App) {
    stop_agents_and_quit(
        "Buzz couldn’t restart",
        move || {
            crate::updates::install::relaunch_after_exit(&program).inspect_err(|error| {
                tracing::error!(%error, "restart for the update was refused");
            })
        },
        cx,
    );
}

/// Stops this computer's agents off the UI thread, runs `then`, and
/// quits. If either fails the app keeps running and shows `title`.
fn stop_agents_and_quit(
    title: &'static str,
    then: impl FnOnce() -> Result<(), String> + 'static,
    cx: &mut App,
) {
    // A second request while agents are stopping joins the first.
    static STOPPING: AtomicBool = AtomicBool::new(false);
    if STOPPING.swap(true, Ordering::SeqCst) {
        return;
    }
    save_geometry(cx);
    let host = crate::hosts::Hosts::embedded(cx);
    cx.spawn(async move |cx| {
        let stopped = cx
            .background_spawn(async move { host.map_or(Ok(()), |host| host.shutdown()) })
            .await
            .map_err(|error| {
                tracing::error!(%error, "agent shutdown incomplete; app exit was refused");
                format!("{error}. Your agents may still be running; try again.")
            });
        cx.update(|cx| match stopped.and_then(|()| then()) {
            Ok(()) => cx.quit(),
            Err(error) => {
                STOPPING.store(false, Ordering::SeqCst);
                crate::ui::toasts::show(
                    crate::ui::toasts::Notice::error("quit", title).description(error),
                    cx,
                );
            }
        });
    })
    .detach();
}

/// Closes the window and keeps running; the tray, a notification or
/// another launch opens it again.
fn hide(window: &mut Window, cx: &mut App) {
    remember_bounds(window.window_bounds(), cx);
    save_geometry(cx);
    window.remove_window();
    Idle::set_visible(false, cx);
}

fn remember_bounds(bounds: WindowBounds, cx: &mut App) {
    if usable(&bounds) {
        cx.global_mut::<MainWindow>().bounds = Some(bounds);
    }
}

/// Saves where the window is for the next launch.
fn save_geometry(cx: &mut App) {
    let Some(bounds) = cx.global::<MainWindow>().bounds else {
        return;
    };
    let json = saved_geometry(bounds).to_string();
    if let Err(error) = cx
        .global::<AppState>()
        .store
        .set_device_pref(GEOMETRY, &json)
    {
        tracing::warn!(%error, "could not save where the window is");
    }
}

/// Follows moves and resizes, saving once they pause.
fn moved(window: &mut Window, cx: &mut App) {
    remember_bounds(window.window_bounds(), cx);
    let main = cx.global_mut::<MainWindow>();
    main.moves += 1;
    let generation = main.moves;
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_GEOMETRY_AFTER).await;
        cx.update(|cx| {
            if cx.global::<MainWindow>().moves == generation {
                save_geometry(cx);
            }
        });
    })
    .detach();
}

/// The window's root: input for automatic away, captured before any
/// control handles it, and the launch surface over the workspace.
struct Root {
    workspace: Entity<Workspace>,
    launch: Option<Launch>,
    _subscriptions: Vec<Subscription>,
}

impl Root {
    fn new(
        workspace: Entity<Workspace>,
        launch: Option<Launch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe_window_bounds(window, |_, window, cx| moved(window, cx)),
            cx.observe_window_activation(window, |_, window, cx| {
                if window.is_window_active() {
                    Idle::input(cx);
                }
            }),
        ];
        Self {
            workspace,
            launch,
            _subscriptions: subscriptions,
        }
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::ui::overlay::begin_frame();
        crate::perf::frame_start();
        let launch = self.launch.as_mut().and_then(|launch| {
            launch.update(self.workspace.read(cx).launch_content(cx), Instant::now());
            launch.render(window, cx)
        });
        if launch.is_none() && self.launch.take().is_some() {
            // The launch surface is gone: the app shows.
            crate::perf::shown(crate::perf::STARTUP, "launch_surface_gone");
        }
        // The surface animates every frame, and nothing under it shows
        // while it does: the workspace draws from its last frame there
        // unless it changed.
        let covered = self.launch.as_ref().is_some_and(Launch::showing);
        let workspace = match covered {
            true => AnyView::from(self.workspace.clone())
                .cached(StyleRefinement::default().size_full())
                .into_any_element(),
            false => self.workspace.clone().into_any_element(),
        };
        div()
            .relative()
            .size_full()
            .capture_any_mouse_down(|_, _, cx| Idle::input(cx))
            .capture_key_down(|_, _, cx| Idle::input(cx))
            .on_mouse_move(|_, _, cx| Idle::input(cx))
            .on_scroll_wheel(|_, _, cx| Idle::input(cx))
            .on_key_down(tab_through)
            .child(workspace)
            .children(launch)
            .when(crate::perf::on(), |root| {
                // Painted last: the end of the frame's CPU work.
                root.child(
                    canvas(|_, _, _| {}, |_, _, _, _| crate::perf::frame_end())
                        .absolute()
                        .size_0(),
                )
            })
    }
}

/// Tab and Shift+Tab move focus through the tab stops, as a browser's
/// do, when nothing focused took the key. A modal keeps focus, and a
/// focus trap (the media viewer) cycles inside itself.
fn tab_through(event: &KeyDownEvent, window: &mut Window, cx: &mut App) {
    let keys = &event.keystroke;
    let modifiers = keys.modifiers;
    if keys.key != "tab" || modifiers.control || modifiers.alt || modifiers.platform {
        return;
    }
    cx.stop_propagation();
    if crate::ui::overlay::modal_shown() {
        return;
    }
    let step = |window: &mut Window, cx: &mut App| {
        if modifiers.shift {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
    };
    let trap = gpui_kit::base::active_focus_trap(window, cx);
    let before = window.focused(cx);
    step(window, cx);
    let Some(trap) = trap else {
        return;
    };
    // Past the trap's last stop: on round to its first.
    for _ in 0..10_000 {
        if trap.contains_focused(window, cx) || window.focused(cx) == before {
            return;
        }
        step(window, cx);
    }
    trap.focus(window, cx);
}

/// Where the window was, as tauri-plugin-window-state 2.4.1 writes it:
/// the windowed bounds, also kept while maximized or fullscreen.
fn saved_geometry(bounds: WindowBounds) -> serde_json::Value {
    let (inner, maximized, fullscreen) = match bounds {
        WindowBounds::Windowed(inner) => (inner, false, false),
        WindowBounds::Maximized(inner) => (inner, true, false),
        WindowBounds::Fullscreen(inner) => (inner, false, true),
    };
    let (x, y) = (f32::from(inner.origin.x), f32::from(inner.origin.y));
    serde_json::json!({ LABEL: {
        "width": f32::from(inner.size.width).round() as u32,
        "height": f32::from(inner.size.height).round() as u32,
        "x": x.round() as i32,
        "y": y.round() as i32,
        "prev_x": x.round() as i32,
        "prev_y": y.round() as i32,
        "maximized": maximized,
        "visible": true,
        "decorated": true,
        "fullscreen": fullscreen,
    }})
}

/// The parts of a saved entry buzz-desktop leaves to the front end: the
/// window mode, and the geometry as saved for when no screen is known.
#[derive(Deserialize)]
struct Mode {
    maximized: bool,
    fullscreen: bool,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    prev_x: i32,
    prev_y: i32,
}

/// The saved geometry, moved so the header and both control areas are on
/// some screen; otherwise at the top of the primary work area
/// (`window_state::restore_request`).
fn restore_geometry(bytes: &[u8], cx: &App) -> Option<WindowBounds> {
    let areas: Vec<_> = cx
        .displays()
        .iter()
        .map(|display| screen_rect(display.visible_bounds()))
        .collect();
    let primary = cx
        .primary_display()
        .map(|display| screen_rect(display.visible_bounds()));
    restore_bounds(bytes, &areas, primary)
}

fn screen_rect(bounds: Bounds<Pixels>) -> buzz_desktop::window_state::Rect {
    use buzz_desktop::window_state::{Position, Rect, Size as Extent};
    Rect {
        position: Position::new(
            f32::from(bounds.origin.x).round() as i32,
            f32::from(bounds.origin.y).round() as i32,
        ),
        size: Extent::new(
            f32::from(bounds.size.width).max(0.).round() as u32,
            f32::from(bounds.size.height).max(0.).round() as u32,
        ),
    }
}

/// [`restore_geometry`] for the given screens' work areas.
fn restore_bounds(
    bytes: &[u8],
    areas: &[buzz_desktop::window_state::Rect],
    primary: Option<buzz_desktop::window_state::Rect>,
) -> Option<WindowBounds> {
    use buzz_desktop::window_state::{self as state, Position, Rect, Size as Extent};
    let saved = state::saved_geometry(bytes, LABEL)?;
    let mode: Mode = serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|mut file| serde_json::from_value(file[LABEL].take()).ok())?;
    let real = |area: &Rect| area.size.width > 0 && area.size.height > 0;
    let areas: Vec<Rect> = areas.iter().copied().filter(real).collect();
    let fallback = primary.filter(real).or_else(|| areas.first().copied());
    // Wayland reports no screens until the event loop has run, which is
    // after this, and places windows itself. Fitting the window to no
    // screen would shrink it to nothing, so it keeps its saved geometry.
    let (origin, extent) = match fallback {
        Some(fallback) => {
            let header = f32::from(HEADER_HEIGHT) as u32;
            state::restore_request(&saved, Position::default(), header, &areas, fallback)
        }
        None if mode.maximized => (
            Position::new(mode.prev_x, mode.prev_y),
            Extent::new(mode.width, mode.height),
        ),
        None => (
            Position::new(mode.x, mode.y),
            Extent::new(mode.width, mode.height),
        ),
    };
    let bounds = Bounds::new(
        point(px(origin.x as f32), px(origin.y as f32)),
        size(px(extent.width as f32), px(extent.height as f32)),
    );
    Some(if mode.fullscreen {
        WindowBounds::Fullscreen(bounds)
    } else if mode.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    })
    .filter(usable)
}

pub fn preferences(cx: &App) -> Preferences {
    cx.global::<MainWindow>().preferences
}

/// Whether a panel is showing the tray icon, so hiding to it is safe.
pub fn tray_shown(cx: &App) -> bool {
    cx.global::<MainWindow>().shown
}

pub fn set_preferences(preferences: Preferences, cx: &mut App) {
    cx.global_mut::<MainWindow>().preferences = preferences;
    let json = serde_json::to_string(&preferences).expect("preferences serialize");
    if let Err(error) = cx
        .global::<AppState>()
        .store
        .set_device_pref(PREFERENCES, &json)
    {
        tracing::warn!(%error, "could not save the tray settings");
    }
    apply_preferences(cx);
}

/// Puts the icon up or takes it down to match the choices.
fn apply_preferences(cx: &mut App) {
    let main = cx.global::<MainWindow>();
    match (main.preferences.show_icon, main.tray.is_some()) {
        (true, false) => start_tray(cx),
        (false, true) => {
            let main = cx.global_mut::<MainWindow>();
            main.tray = None;
            main.shown = false;
            cx.refresh_windows();
        }
        _ => {}
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn start_tray(cx: &mut App) {
    use futures::StreamExt;
    let Some(navigation) = navigation::sender(cx) else {
        return;
    };
    let (events, mut received) = futures::channel::mpsc::unbounded();
    let runtime = Runtime::handle(cx);
    let started = runtime.spawn(Tray::start(navigation, events));
    cx.spawn(async move |cx| {
        let tray = match started.await {
            Ok(Ok(tray)) => Arc::new(tray),
            Ok(Err(error)) => {
                tracing::warn!(%error, "could not put up the tray icon");
                return;
            }
            Err(_) => return,
        };
        let kept = cx.update(|cx| {
            let main = cx.global_mut::<MainWindow>();
            // Turned off while it was starting.
            if !main.preferences.show_icon || main.tray.is_some() {
                return false;
            }
            main.tray = Some(tray.clone());
            main.attention = u32::MAX;
            true
        });
        if !kept {
            return;
        }
        cx.update(update_attention);
        let weak = Arc::downgrade(&tray);
        drop(tray);
        while let Some(event) = received.next().await {
            // Taken down: the events of the old icon no longer count.
            let current = cx.update(|cx| {
                cx.global::<MainWindow>()
                    .tray
                    .as_ref()
                    .zip(weak.upgrade())
                    .is_some_and(|(now, this)| Arc::ptr_eq(now, &this))
            });
            if !current {
                return;
            }
            cx.update(|cx| match event {
                TrayEvent::Shown(shown) => {
                    cx.global_mut::<MainWindow>().shown = shown;
                    cx.refresh_windows();
                }
                TrayEvent::Quit => quit(cx),
            });
        }
    })
    .detach();
}

#[cfg(any(target_os = "macos", windows))]
fn start_tray(cx: &mut App) {
    use futures::StreamExt;
    let Some(navigation) = navigation::sender(cx) else {
        return;
    };
    let (events, mut received) = futures::channel::mpsc::unbounded();
    let tray = match Tray::start(navigation, events) {
        Ok(tray) => tray,
        Err(error) => {
            tracing::warn!(%error, "could not put up the tray icon");
            return;
        }
    };
    cx.global_mut::<MainWindow>().tray = Some(tray);
    update_attention(cx);
    // Ends when the icon is taken down, which drops its senders.
    cx.spawn(async move |cx| {
        while let Some(event) = received.next().await {
            cx.update(|cx| match event {
                TrayEvent::Shown(shown) => {
                    cx.global_mut::<MainWindow>().shown = shown;
                    cx.refresh_windows();
                }
                TrayEvent::Quit => quit(cx),
            });
        }
    })
    .detach();
}

/// Follows every community's unread counts for the tray icon.
fn watch_communities(cx: &mut App) {
    let communities = cx.global::<AppState>().communities.clone();
    let subscriptions = communities
        .iter()
        .map(|community| cx.observe(community, |_, cx| update_attention(cx)))
        .collect();
    cx.global_mut::<MainWindow>()._communities = subscriptions;
    update_attention(cx);
}

fn attention(communities: &[Entity<Community>], cx: &App) -> u32 {
    communities
        .iter()
        .map(|community| snapshot_attention(&community.read(cx).snapshot))
        .sum()
}

/// Unread mentions and direct messages in the rows the sidebar lists.
fn snapshot_attention(snapshot: &SessionSnapshot) -> u32 {
    crate::ui::sidebar::listed_unread(snapshot)
        .filter(|(_, unread)| unread.unread)
        .map(|(channel, unread)| match channel.channel_type {
            Some(ChannelType::Dm) => unread.count,
            _ => unread.mentions,
        })
        .sum()
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn update_attention(cx: &mut App) {
    let count = attention(&cx.global::<AppState>().communities.clone(), cx);
    let main = cx.global_mut::<MainWindow>();
    let Some(tray) = main.tray.clone() else {
        return;
    };
    if main.attention == count {
        return;
    }
    main.attention = count;
    Runtime::handle(cx).spawn(async move {
        if let Err(error) = tray.set_attention(count).await {
            tracing::debug!(%error, "could not update the tray icon");
        }
    });
}

#[cfg(any(target_os = "macos", windows))]
fn update_attention(cx: &mut App) {
    let count = attention(&cx.global::<AppState>().communities.clone(), cx);
    if let Some(tray) = cx.global_mut::<MainWindow>().tray.as_mut()
        && let Err(error) = tray.set_attention(count)
    {
        tracing::debug!(%error, "could not update the tray icon");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn the_tray_counts_mentions_and_direct_messages_in_listed_rows() {
        use buzz_relay_client::sidebar_store::SidebarState;
        use buzz_relay_client::{ChannelSummary, ChannelUnread};
        let channel = |id: &str, kind: ChannelType| ChannelSummary {
            id: id.into(),
            channel_type: Some(kind),
            ..Default::default()
        };
        let row = |count, mentions| ChannelUnread {
            unread: true,
            count,
            mentions,
            ..Default::default()
        };
        let channels = vec![
            channel("general", ChannelType::Stream),
            channel("dm", ChannelType::Dm),
            channel("hidden-dm", ChannelType::Dm),
        ];
        let rows = [
            ("general", row(5, 1)),
            ("dm", row(2, 0)),
            ("hidden-dm", row(3, 0)),
        ];
        let sidebar = SidebarState {
            hidden_dms: vec!["hidden-dm".into()],
            ..Default::default()
        };
        let snapshot = crate::ui::sidebar::unread_snapshot(channels, &rows, sidebar);
        assert_eq!(snapshot_attention(&snapshot), 3);
    }

    #[test]
    fn geometry_is_saved_as_window_state_writes_it() {
        let inner = Bounds::new(point(px(10.), px(20.)), size(px(900.), px(700.)));
        let json = saved_geometry(WindowBounds::Maximized(inner)).to_string();
        let saved = buzz_desktop::window_state::saved_geometry(json.as_bytes(), LABEL).unwrap();
        let screen = buzz_desktop::window_state::Rect {
            position: Default::default(),
            size: buzz_desktop::window_state::Size::new(1920, 1080),
        };
        let (origin, extent) = buzz_desktop::window_state::restore_request(
            &saved,
            Default::default(),
            48,
            &[screen],
            screen,
        );
        assert_eq!(
            (origin.x, origin.y, extent.width, extent.height),
            (10, 20, 900, 700)
        );
        let file: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(file[LABEL]["maximized"], true);
        assert_eq!(file[LABEL]["fullscreen"], false);
    }

    #[test]
    fn restores_the_saved_size_before_any_screen_is_known() {
        use buzz_desktop::window_state::{Position, Rect, Size as Extent};
        let inner = Bounds::new(point(px(0.), px(0.)), size(px(1200.), px(800.)));
        let json = saved_geometry(WindowBounds::Windowed(inner)).to_string();
        let size_of = |bounds: Option<WindowBounds>| {
            let size = bounds.unwrap().get_bounds().size;
            (f32::from(size.width), f32::from(size.height))
        };
        // Wayland at launch: no screens yet, or ones with no size.
        assert_eq!(
            size_of(restore_bounds(json.as_bytes(), &[], None)),
            (1200., 800.)
        );
        let empty = Rect::default();
        assert_eq!(
            size_of(restore_bounds(json.as_bytes(), &[empty], Some(empty))),
            (1200., 800.)
        );
        // A known screen too small for it still shrinks it to fit.
        let small = Rect {
            position: Position::new(0, 0),
            size: Extent::new(1000, 700),
        };
        assert_eq!(
            size_of(restore_bounds(json.as_bytes(), &[], Some(small))),
            (1000., 700.)
        );
    }

    #[test]
    fn a_window_too_small_to_see_is_not_kept() {
        let at = |w: f32, h: f32| Bounds::new(point(px(0.), px(0.)), size(px(w), px(h)));
        // What a hidden window once left behind: restored, no one saw it.
        assert!(!usable(&WindowBounds::Windowed(at(1., 1.))));
        assert!(!usable(&WindowBounds::Maximized(at(479., 800.))));
        assert!(usable(&WindowBounds::Windowed(at(480., 400.))));
        assert!(usable(&WindowBounds::Fullscreen(at(1920., 1080.))));
    }

    #[test]
    fn preferences_default_to_an_icon_without_hiding() {
        let parsed: Preferences = serde_json::from_str(r#"{"close_to_tray":true}"#).unwrap();
        assert!(parsed.show_icon && parsed.close_to_tray && !parsed.minimize_to_tray);
        assert!(!Preferences::default().close_to_tray);
    }
}
