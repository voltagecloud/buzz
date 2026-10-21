//! The tray icon: a StatusNotifierItem with a small menu
//! (`com.canonical.dbusmenu`), which KDE, Ubuntu's AppIndicator extension
//! and most other Linux panels show.
//!
//! Clicking the icon or "Open Buzz" brings the window back; with
//! close-to-tray or minimize-to-tray the window may be closed while the app
//! keeps running. Hosts that support it hand over an activation token
//! first (`ProvideXdgActivationToken`), which is what lets GNOME raise the
//! window. The icon asks for attention while mentions or direct messages
//! are unread.

use super::{TrayEvent, icon, tool_tip};
use crate::ui::navigation::{Arrival, Request};
use futures::channel::mpsc::UnboundedSender;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";

/// The icon's menu: (id, label). Id 0 is the root.
const OPEN: i32 = 1;
const SEPARATOR: i32 = 2;
const QUIT: i32 = 3;

/// The tray icon while it is up; dropping it takes the icon down.
pub struct Tray {
    /// Holds a copy of the connection, so it is stopped first: the
    /// connection closes, and its name goes, once the last copy is dropped.
    _watcher: zbus::Task<()>,
    connection: zbus::Connection,
}

impl Tray {
    /// Puts the icon up. Requests to open go to navigation; the rest come
    /// back as [`TrayEvent`]s.
    pub async fn start(
        navigation: UnboundedSender<Arrival>,
        events: UnboundedSender<TrayEvent>,
    ) -> zbus::Result<Self> {
        let token = Arc::new(Mutex::new(None));
        let open = {
            let token = token.clone();
            let navigation = navigation.clone();
            Arc::new(move || {
                let activation_token = token.lock().unwrap_or_else(|p| p.into_inner()).take();
                let _ = navigation.unbounded_send(Arrival {
                    request: Request::Focus,
                    activation_token,
                });
            }) as Arc<dyn Fn() + Send + Sync>
        };
        let item = Item {
            token,
            open: open.clone(),
            attention: 0,
        };
        let menu = Menu {
            open,
            events: events.clone(),
        };
        // Unique per process and icon, as the specification asks, so an
        // icon put up again never waits for the last one's name to go.
        static ICONS: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "org.kde.StatusNotifierItem-{}-{}",
            std::process::id(),
            ICONS.fetch_add(1, Ordering::Relaxed) + 1
        );
        let connection = zbus::connection::Builder::session()?
            .name(name.clone())?
            .serve_at(ITEM_PATH, item)?
            .serve_at(MENU_PATH, menu)?
            .build()
            .await?;
        let shown = register(&connection, &name).await;
        let _ = events.unbounded_send(TrayEvent::Shown(shown));
        // Panels come and go (GNOME extensions reload, Plasma restarts):
        // register again whenever a watcher appears.
        let watch = connection.clone();
        let watcher = connection.executor().spawn(
            async move {
                use futures::StreamExt;
                let Ok(bus) = zbus::fdo::DBusProxy::new(&watch).await else {
                    return;
                };
                let Ok(mut changes) = bus.receive_name_owner_changed().await else {
                    return;
                };
                while let Some(change) = changes.next().await {
                    let Ok(args) = change.args() else { continue };
                    if args.name().as_str() != WATCHER {
                        continue;
                    }
                    let shown = args.new_owner().is_some() && register(&watch, &name).await;
                    let _ = events.unbounded_send(TrayEvent::Shown(shown));
                }
            },
            "tray-watcher",
        );
        Ok(Self {
            _watcher: watcher,
            connection,
        })
    }

    /// Asks for attention while `count` mentions and direct messages are
    /// unread.
    pub async fn set_attention(&self, count: u32) -> zbus::Result<()> {
        let item = self
            .connection
            .object_server()
            .interface::<_, Item>(ITEM_PATH)
            .await?;
        let mut state = item.get_mut().await;
        if state.attention == count {
            return Ok(());
        }
        let was = state.current_status();
        state.attention = count;
        let now = state.current_status();
        drop(state);
        let emitter = item.signal_emitter();
        if was != now {
            Item::new_status(emitter, now).await?;
        }
        Item::new_tool_tip(emitter).await
    }
}

async fn register(connection: &zbus::Connection, name: &str) -> bool {
    let call = connection
        .call_method(
            Some(WATCHER),
            WATCHER_PATH,
            Some(WATCHER),
            "RegisterStatusNotifierItem",
            &(name,),
        )
        .await;
    match call {
        Ok(_) => true,
        Err(error) => {
            tracing::info!(%error, "no panel shows tray icons");
            false
        }
    }
}

struct Item {
    /// From `ProvideXdgActivationToken`, for the next activation.
    token: Arc<Mutex<Option<String>>>,
    open: Arc<dyn Fn() + Send + Sync>,
    attention: u32,
}

impl Item {
    fn current_status(&self) -> &'static str {
        if self.attention > 0 {
            "NeedsAttention"
        } else {
            "Active"
        }
    }
}

/// ARGB32 in network byte order, as the specification wants.
type Pixmap = (i32, i32, Vec<u8>);

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "Communications"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "buzz-gpui"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "Buzz"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        self.current_status()
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    /// The installed icon, by name; the pixmaps cover panels that can't
    /// find it.
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "buzz-gpui"
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<Pixmap> {
        icons::normal()
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<Pixmap> {
        icons::attention()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<Pixmap>, String, String) {
        (
            String::new(),
            Vec::new(),
            "Buzz".into(),
            tool_tip(self.attention),
        )
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).expect("a valid object path")
    }

    fn activate(&self, _x: i32, _y: i32) {
        (self.open)();
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        (self.open)();
    }

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    fn provide_xdg_activation_token(&self, token: String) {
        *self.token.lock().unwrap_or_else(|p| p.into_inner()) = Some(token);
    }

    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct Menu {
    open: Arc<dyn Fn() + Send + Sync>,
    events: UnboundedSender<TrayEvent>,
}

/// One menu entry and its children, as `GetLayout` returns it.
#[derive(Clone, Debug, serde::Serialize, zbus::zvariant::Type, zbus::zvariant::Value)]
struct Layout {
    id: i32,
    properties: HashMap<String, OwnedValue>,
    children: Vec<OwnedValue>,
}

fn properties(id: i32) -> HashMap<String, OwnedValue> {
    let entries: Vec<(&str, Value<'static>)> = match id {
        0 => vec![("children-display", Value::from("submenu"))],
        OPEN => vec![("label", Value::from("Open Buzz"))],
        SEPARATOR => vec![("type", Value::from("separator"))],
        QUIT => vec![("label", Value::from("Quit Buzz"))],
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .filter_map(|(name, value)| Some((name.to_string(), OwnedValue::try_from(value).ok()?)))
        .collect()
}

fn layout(id: i32, depth: i32) -> Layout {
    let children = if id == 0 && depth != 0 {
        [OPEN, SEPARATOR, QUIT]
            .into_iter()
            .filter_map(|child| OwnedValue::try_from(Value::from(layout(child, depth - 1))).ok())
            .collect()
    } else {
        Vec::new()
    };
    Layout {
        id,
        properties: properties(id),
        children,
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    /// The menu never changes, so its revision stays 1.
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, Layout) {
        (1, layout(parent_id, recursion_depth))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let ids = if ids.is_empty() {
            vec![0, OPEN, SEPARATOR, QUIT]
        } else {
            ids
        };
        ids.into_iter().map(|id| (id, properties(id))).collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        properties(id)
            .remove(&name)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no property {name}")))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        match id {
            OPEN => (self.open)(),
            QUIT => {
                let _ = self.events.unbounded_send(TrayEvent::Quit);
            }
            _ => {}
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event_id, data, timestamp) in events {
            self.event(id, event_id, data, timestamp);
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

/// The app icon as pixmaps, plain and with an attention dot.
mod icons {
    use super::Pixmap;
    use std::sync::LazyLock;

    /// Panels pick the closest size and scale it.
    const SIZES: [u32; 3] = [22, 32, 64];

    static NORMAL: LazyLock<Vec<Pixmap>> = LazyLock::new(|| render(false));
    static ATTENTION: LazyLock<Vec<Pixmap>> = LazyLock::new(|| render(true));

    pub fn normal() -> Vec<Pixmap> {
        NORMAL.clone()
    }

    pub fn attention() -> Vec<Pixmap> {
        ATTENTION.clone()
    }

    fn render(dot: bool) -> Vec<Pixmap> {
        SIZES
            .iter()
            .filter_map(|&size| {
                let icon = super::icon(size, dot)?;
                let argb = icon
                    .pixels()
                    .flat_map(|pixel| {
                        let [r, g, b, a] = pixel.0;
                        [a, r, g, b]
                    })
                    .collect();
                Some((size as i32, size as i32, argb))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_layout_lists_open_and_quit() {
        let root = layout(0, -1);
        assert_eq!(root.children.len(), 3);
        let labels: Vec<Option<String>> = [OPEN, SEPARATOR, QUIT]
            .into_iter()
            .map(|id| {
                properties(id)
                    .get("label")
                    .and_then(|value| String::try_from(value.clone()).ok())
            })
            .collect();
        assert_eq!(
            labels,
            [Some("Open Buzz".into()), None, Some("Quit Buzz".into())]
        );
        // Depth 0 asks for the item alone.
        assert!(layout(0, 0).children.is_empty());
    }

    #[test]
    fn icons_are_argb_and_the_dot_shows() {
        let normal = icons::normal();
        let attention = icons::attention();
        assert_eq!(normal.len(), 3);
        for ((w, h, bytes), (_, _, marked)) in normal.iter().zip(&attention) {
            assert_eq!(bytes.len(), (*w * *h * 4) as usize);
            assert_ne!(bytes, marked);
        }
    }
}
