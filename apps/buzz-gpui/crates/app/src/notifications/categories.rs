//! Notification categories contributed by plugins (`notifications.register`
//! in `features/notifications/service.ts`). Each gets its own switch in
//! Settings → Notifications and submits through the host: the viewer's
//! preferences, a dedupe per source key and the reference text rules
//! (`pluginNotificationText`).

use super::{Notifications, plain_text, short_text, show};
use crate::ui::navigation::Target;
use gpui_kit::{App, Global};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a source key keeps a repeat from notifying (`FRESH_MS`).
const FRESH: Duration = Duration::from_secs(120);
/// The most source keys remembered at once.
const MAX_SEEN: usize = 2048;

/// A category a plugin registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CategorySpec {
    /// `plugin/category`, which keys its saved switch.
    pub key: &'static str,
    pub label: &'static str,
}

#[derive(Default)]
struct Categories(Vec<CategorySpec>);

impl Global for Categories {}

/// Source keys shown lately, by community, category and key, with when
/// each may notify again.
#[derive(Default)]
struct Seen(HashMap<(String, String, String), Instant>);

impl Global for Seen {}

/// Adds a category, replacing one with the same key.
pub fn register(spec: CategorySpec, cx: &mut App) {
    let categories = &mut cx.default_global::<Categories>().0;
    categories.retain(|existing| existing.key != spec.key);
    categories.push(spec);
}

/// Removes the category `key`, as when the plugin that added it stops.
pub fn unregister(key: &str, cx: &mut App) {
    cx.default_global::<Categories>()
        .0
        .retain(|existing| existing.key != key);
}

/// The categories whose plugin is on, in registration order.
pub fn all(cx: &App) -> Vec<CategorySpec> {
    cx.try_global::<Categories>()
        .map(|categories| categories.0.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|spec| crate::plugins::enabled_key(spec.key))
        .collect()
}

/// One plugin notification.
pub struct Submission {
    /// Unique per thing notified about; a repeat within two minutes is
    /// dropped.
    pub source_key: String,
    pub title: String,
    pub body: String,
    pub target: Target,
}

/// The reference text: a 128-character title or "Buzz", and the body as
/// plain text or "New <label>".
fn text(submission: &Submission, label: &str) -> (String, String) {
    let title = short_text(&submission.title, 128);
    let body = plain_text(&submission.body);
    (
        if title.is_empty() {
            "Buzz".into()
        } else {
            title
        },
        if body.is_empty() {
            format!("New {}", label.to_lowercase())
        } else {
            body
        },
    )
}

/// Shows `submission` if alerts and its category are on; false if it was
/// dropped.
pub fn submit(key: &str, submission: Submission, cx: &mut App) -> bool {
    let Some(spec) = all(cx).into_iter().find(|spec| spec.key == key) else {
        return false;
    };
    let preferences = Notifications::preferences(cx);
    if !preferences.enabled || preferences.categories.get(key) == Some(&false) {
        return false;
    }
    // Signed out, nothing notifies.
    if !matches!(
        cx.global::<crate::community::AppState>().identity,
        crate::community::IdentityState::Ready(_)
    ) {
        return false;
    }
    let scope = submission
        .target
        .community
        .clone()
        .unwrap_or_else(|| "local".to_owned());
    let seen = (scope, key.to_owned(), submission.source_key.clone());
    let now = Instant::now();
    let remembered = &mut cx.default_global::<Seen>().0;
    remembered.retain(|_, until| *until > now);
    if remembered.contains_key(&seen) {
        return false;
    }
    remembered.insert(seen, now + FRESH);
    while remembered.len() > MAX_SEEN {
        let Some(oldest) = remembered
            .iter()
            .min_by_key(|(_, until)| **until)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        remembered.remove(&oldest);
    }
    let (title, body) = text(&submission, spec.label);
    let shown = show(title, body, submission.target, None, cx);
    cx.spawn(async move |cx| {
        let result = shown.await.unwrap_or(Ok(()));
        cx.update(|cx| match result {
            Ok(()) => crate::sounds::play_default(&Notifications::preferences(cx)),
            Err(error) => Notifications::report(error, cx),
        });
    })
    .detach();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::navigation::Place;
    use core::prelude::v1::test;

    fn submission(title: &str, body: &str) -> Submission {
        Submission {
            source_key: "a:1".into(),
            title: title.into(),
            body: body.into(),
            target: Target {
                community: None,
                place: Place::Page(crate::ui::pages::Page::MESSAGES),
            },
        }
    }

    #[test]
    fn text_falls_back_like_plugin_notification_text() {
        assert_eq!(
            text(&submission("", "  "), "Reminders"),
            ("Buzz".into(), "New reminders".into())
        );
        assert_eq!(
            text(
                &submission("Reminder due · #general", "**Call** Bob"),
                "Reminders"
            ),
            ("Reminder due · #general".into(), "Call Bob".into())
        );
    }
}
