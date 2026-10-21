//! Banners through the system's notification center on macOS and Windows,
//! which GPUI posts: `UNUserNotificationCenter` and WinRT toasts. Both are
//! silent; the app plays the chosen sound itself.
//!
//! Each banner is tagged with a fresh ID, and a click comes back with that
//! tag, so the banner's place is remembered for recent banners. macOS asks
//! for permission on the first banner, and posts nothing for a binary that
//! is not inside `Buzz.app`. A message's banner is removed from the
//! notification center by its tag once the message is read. Windows shows toasts under the app identity
//! set at launch (`main`), the macOS bundle identifier.

use super::Key;
use crate::ui::navigation::{Arrival, Request, Target};
use futures::channel::mpsc::UnboundedSender;
use gpui_kit::{App, Global, SharedString, SystemNotification};

/// Banners shown and not yet clicked, and the messages they are about.
#[derive(Default)]
struct Shown(super::shown::Shown<SharedString>);

impl Global for Shown {}

impl Shown {
    fn insert(&mut self, tag: SharedString, target: Target, key: Option<Key>) {
        self.0.insert(tag, target, key);
    }

    fn clicked(&mut self, tag: &SharedString) -> Option<Target> {
        self.0.forget(tag)
    }

    /// The tags of the banners about `key`'s message, forgotten.
    fn withdraw(&mut self, key: &Key) -> Vec<SharedString> {
        let tags = self.0.withdraw(key);
        for tag in &tags {
            self.0.forget(tag);
        }
        tags
    }
}

/// Whether the system will show banners for this process: on macOS only
/// from inside an app bundle.
pub fn available() -> bool {
    !cfg!(target_os = "macos")
        || std::env::current_exe()
            .is_ok_and(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
}

/// Sends clicked banners' places to navigation. Call once, before the
/// first [`show`].
pub fn start(navigation: UnboundedSender<Arrival>, cx: &mut App) {
    cx.set_global(Shown::default());
    cx.on_system_notification_response(move |response, cx| {
        // Action buttons are not offered; only the banner itself.
        if response.action_id.is_some() {
            return;
        }
        if let Some(target) = cx.global_mut::<Shown>().clicked(&response.tag) {
            let _ = navigation.unbounded_send(Request::Open(target).into());
        }
    });
}

/// Posts one banner. The system shows it, or not, on its own; only a
/// process macOS will not show banners for is refused here.
pub fn show(
    title: String,
    body: String,
    target: Target,
    key: Option<Key>,
    cx: &mut App,
) -> Result<(), String> {
    if !available() {
        return Err("Notifications need Buzz to run from Buzz.app.".into());
    }
    let tag = SharedString::from(uuid::Uuid::new_v4().to_string());
    cx.global_mut::<Shown>().insert(tag.clone(), target, key);
    cx.show_system_notification(SystemNotification {
        tag,
        title: title.into(),
        body: body.into(),
        actions: Vec::new(),
    });
    Ok(())
}

/// Removes the banners about `key`'s message from the notification
/// center.
pub fn withdraw(key: &Key, cx: &mut App) {
    let tags = cx.global_mut::<Shown>().withdraw(key);
    for tag in tags {
        cx.dismiss_system_notification(&tag);
    }
}

#[cfg(test)]
mod tests {
    use super::super::shown::REMEMBERED;
    use super::*;
    use crate::ui::navigation::Place;

    fn target(channel: &str) -> Target {
        Target {
            community: None,
            place: Place::Conversation {
                channel: channel.into(),
                message: None,
                thread: None,
                members: false,
            },
        }
    }

    fn key(message: u8) -> Key {
        Key {
            origin: "ws://relay".into(),
            channel: "general".into(),
            message: buzz_relay_client::EventId::from_byte_array([message; 32]),
            thread: None,
        }
    }

    #[test]
    fn a_read_message_removes_its_banner_once() {
        let mut shown = Shown::default();
        shown.insert("a".into(), target("one"), Some(key(1)));
        shown.insert("b".into(), target("one"), Some(key(2)));
        assert_eq!(shown.withdraw(&key(1)), [SharedString::from("a")]);
        assert!(shown.withdraw(&key(1)).is_empty());
        assert_eq!(shown.clicked(&"a".into()), None);
        // A clicked banner is not removed again.
        assert!(shown.clicked(&"b".into()).is_some());
        assert!(shown.withdraw(&key(2)).is_empty());
    }

    #[test]
    fn a_click_opens_the_banner_once() {
        let mut shown = Shown::default();
        shown.insert("a".into(), target("one"), None);
        assert_eq!(shown.clicked(&"a".into()), Some(target("one")));
        assert_eq!(shown.clicked(&"a".into()), None);
    }

    #[test]
    fn only_recent_banners_are_remembered() {
        let mut shown = Shown::default();
        for n in 0..=REMEMBERED {
            shown.insert(n.to_string().into(), target("x"), None);
        }
        assert_eq!(shown.clicked(&"0".into()), None);
        assert!(shown.clicked(&REMEMBERED.to_string().into()).is_some());
    }
}
