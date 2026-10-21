//! The window's toast region (`ToastProvider` in buzz-app's `Toast.tsx`):
//! a column of toast cards at the bottom right, newest first. Anything in
//! the app can raise one through [`show`]; a keyed toast replaces the one
//! with the same key in place, as a `ToastNotice` updates.

use buzz_ui::{Button, ButtonSize, Surface, Toast, ToastTone};
use gpui_kit::*;
use std::rc::Rc;
use std::time::Duration;

pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One toast button.
#[derive(Clone)]
pub struct Action {
    pub label: SharedString,
    pub run: Handler,
}

impl Action {
    pub fn new(
        label: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            run: Rc::new(run),
        }
    }
}

/// A toast to raise.
#[derive(Clone)]
pub struct Notice {
    /// Raising another toast with the same key replaces this one.
    pub key: SharedString,
    pub title: SharedString,
    pub description: Option<SharedString>,
    pub tone: ToastTone,
    /// `None` keeps it until it is dismissed or its source resolves.
    pub timeout: Option<Duration>,
    /// Shows the close button.
    pub dismissible: bool,
    pub actions: Vec<Action>,
}

impl Notice {
    /// An error that stays until dismissed.
    pub fn error(key: impl Into<SharedString>, title: impl Into<SharedString>) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            description: None,
            tone: ToastTone::Error,
            timeout: None,
            dismissible: true,
            actions: Vec::new(),
        }
    }

    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }
}

#[derive(Default)]
struct Toasts {
    shown: Vec<(Notice, usize)>,
    raised: usize,
}

impl Global for Toasts {}

/// Raises `notice`, replacing one with the same key.
pub fn show(notice: Notice, cx: &mut App) {
    let toasts = cx.default_global::<Toasts>();
    toasts.raised += 1;
    let generation = toasts.raised;
    let timeout = notice.timeout;
    let key = notice.key.clone();
    upsert(&mut toasts.shown, notice, generation);
    if let Some(timeout) = timeout {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(timeout).await;
            cx.update(|cx| {
                let toasts = cx.default_global::<Toasts>();
                let before = toasts.shown.len();
                toasts
                    .shown
                    .retain(|(n, g)| !(n.key == key && *g == generation));
                if toasts.shown.len() != before {
                    cx.refresh_windows();
                }
            });
        })
        .detach();
    }
    cx.refresh_windows();
}

/// Puts `notice` in the place of the one with its key, else last.
fn upsert(shown: &mut Vec<(Notice, usize)>, notice: Notice, generation: usize) {
    match shown.iter_mut().find(|(n, _)| n.key == notice.key) {
        Some(slot) => *slot = (notice, generation),
        None => shown.push((notice, generation)),
    }
}

/// Takes down the toast with `key`, if it is up.
pub fn dismiss(key: &str, cx: &mut App) {
    if !cx.has_global::<Toasts>() {
        return;
    }
    let toasts = cx.global_mut::<Toasts>();
    let before = toasts.shown.len();
    toasts.shown.retain(|(notice, _)| notice.key != key);
    if toasts.shown.len() != before {
        cx.refresh_windows();
    }
}

/// The card for one notice.
pub fn card(notice: &Notice, generation: usize) -> Toast {
    let key = notice.key.clone();
    let mut toast = Toast::new(
        SharedString::from(format!("toast-{key}-{generation}")),
        notice.title.clone(),
    )
    .tone(notice.tone);
    if let Some(description) = &notice.description {
        toast = toast.description(description.clone());
    }
    for (index, action) in notice.actions.iter().enumerate() {
        let run = action.run.clone();
        toast = toast.action(
            Button::new(
                SharedString::from(format!("toast-{key}-action-{index}")),
                action.label.clone(),
            )
            .size(ButtonSize::Sm)
            .surface(Surface::Floating)
            .on_click(move |_, window, cx| run(window, cx)),
        );
    }
    if notice.dismissible {
        toast = toast.on_dismiss(move |_, _, cx| dismiss(&key, cx));
    }
    toast
}

/// The region's inset from the window's right and bottom edges
/// (`--space-4`, and Sonner's `offset` of `--space-6`).
const RIGHT: Pixels = px(16.);
const BOTTOM: Pixels = px(24.);

/// Lays `cards`, oldest first, out in the toast region above dialogs:
/// newest on top, 24rem wide at most, and scrolling past 40% of the
/// window. The region's 12px padding leaves room for the cards' shadow.
pub fn stack(window: &Window, mut cards: Vec<AnyElement>) -> Option<AnyElement> {
    if cards.is_empty() {
        return None;
    }
    cards.reverse();
    let size = window.viewport_size();
    let width = (window.rem_size() * 24.).min(size.width - RIGHT * 2.);
    let height = (size.height * 0.4).min(size.height - BOTTOM * 2.);
    Some(
        deferred(
            anchored()
                .position_mode(AnchoredPositionMode::Window)
                .anchor(Anchor::BottomRight)
                .position(point(size.width - RIGHT, size.height - BOTTOM))
                .child(
                    div()
                        .id("toasts")
                        .flex()
                        .flex_col()
                        .gap(rems(0.75))
                        .w(width)
                        .max_h(height)
                        .p(rems(0.75))
                        .overflow_y_scroll()
                        // Only the cards take clicks, not the gaps.
                        .children(cards.into_iter().map(|card| div().occlude().child(card))),
                ),
        )
        .with_priority(3)
        .into_any_element(),
    )
}

/// One card in the toast region.
pub fn place(window: &Window, card: impl IntoElement) -> AnyElement {
    stack(window, vec![card.into_any_element()]).expect("one card")
}

/// The window's toasts.
pub fn render(window: &Window, cx: &App) -> Option<AnyElement> {
    let toasts = cx.try_global::<Toasts>()?;
    let cards = toasts
        .shown
        .iter()
        .map(|(notice, generation)| card(notice, *generation).into_any_element())
        .collect();
    stack(window, cards)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn a_key_replaces_its_toast() {
        let mut shown = Vec::new();
        upsert(&mut shown, Notice::error("save", "First"), 1);
        upsert(&mut shown, Notice::error("other", "Other"), 2);
        upsert(&mut shown, Notice::error("save", "Second"), 3);
        let titles: Vec<_> = shown
            .iter()
            .map(|(n, g)| (n.title.to_string(), *g))
            .collect();
        assert_eq!(
            titles,
            [("Second".to_string(), 3), ("Other".to_string(), 2)]
        );
    }
}
