//! Settings → Shortcuts (§5b.5 item 4; `ShortcutSettings.tsx`): every
//! host and plugin shortcut from the live registry, grouped by owner, rebound by pressing a new chord
//! (`KeyCaptureControl.tsx`), with per-row and global resets and the
//! save error's retry.

use super::{button, header, inline_header, preference_row, rows_group, section};
use crate::shortcuts::{self, APPLE, Accepted, DESKTOP, Press, Row, Shortcuts};
use buzz_ui::{
    ActiveTheme, ButtonSize, IconButton, IconButtonSize, IconName, Kbd, Surface, TypeStyled,
    tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Error,
    Warning,
}

/// A line under one row: why a chord was refused, or a warning about
/// the one just saved.
struct Notice {
    key: &'static str,
    tone: Tone,
    message: SharedString,
}

pub struct ShortcutSettings {
    /// The row listening for a new chord.
    editing: Option<&'static str>,
    notice: Option<Notice>,
    /// The capture control's focus; losing it stops listening.
    focus: FocusHandle,
    /// Takes keystrokes before the keymap while listening, so the
    /// shortcut being rebound never fires.
    interceptor: Option<Subscription>,
    _subscriptions: [Subscription; 2],
}

impl ShortcutSettings {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |this, _, cx| {
            if let Some(key) = this.editing {
                this.cancel(key, cx);
            }
        });
        let saved = cx.observe_global::<Shortcuts>(|_, cx| cx.notify());
        Self {
            editing: None,
            notice: None,
            focus,
            interceptor: None,
            _subscriptions: [blur, saved],
        }
    }

    fn start(&mut self, key: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = Some(key);
        self.notice = None;
        window.focus(&self.focus, cx);
        if self.interceptor.is_none() {
            let focus = self.focus.clone();
            let this = cx.entity().downgrade();
            self.interceptor = Some(cx.intercept_keystrokes(move |event, window, cx| {
                if !focus.is_focused(window) {
                    return;
                }
                let press = shortcuts::press(&event.keystroke, APPLE);
                if press == Press::Leave {
                    return;
                }
                cx.stop_propagation();
                let _ = this.update(cx, |this, cx| this.press(press, window, cx));
            }));
        }
        cx.notify();
    }

    /// Stops listening on `key`; its refusal goes with it.
    fn cancel(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if self.editing == Some(key) {
            self.editing = None;
            self.interceptor = None;
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.key == key && notice.tone == Tone::Error)
        {
            self.notice = None;
        }
        cx.notify();
    }

    fn press(&mut self, press: Press, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.editing else {
            return;
        };
        let captured = match press {
            Press::Ignore | Press::Leave => return,
            Press::Cancel => {
                self.cancel(key, cx);
                window.blur(cx);
                return;
            }
            Press::Chord(captured) => captured,
        };
        let rows = shortcuts::rows(Shortcuts::overrides(cx), DESKTOP);
        let Some(target) = rows.iter().find(|row| row.key == key) else {
            return;
        };
        match shortcuts::accept(target, &captured, &rows, APPLE, DESKTOP) {
            Err(message) => {
                self.notice = Some(Notice {
                    key,
                    tone: Tone::Error,
                    message: message.into(),
                });
            }
            Ok(Accepted { binding, warning }) => {
                self.editing = None;
                self.interceptor = None;
                self.notice = warning.map(|message| Notice {
                    key,
                    tone: Tone::Warning,
                    message: message.into(),
                });
                window.blur(cx);
                Shortcuts::set(key, binding, cx);
            }
        }
        cx.notify();
    }

    fn reset(&mut self, key: &'static str, cx: &mut Context<Self>) {
        self.cancel(key, cx);
        self.notice = None;
        Shortcuts::set(key, None, cx);
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.interceptor = None;
        self.notice = None;
        Shortcuts::reset(cx);
    }

    fn row(&self, row: &Row, rows: &[Row], window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let key = row.key;
        let listening = self.editing == Some(key);
        let shared = shortcuts::shared_with(rows, row);
        let subtitle = if row.read_only {
            Some("Reserved by Buzz".into_any_element())
        } else if row.override_.is_some() || !shared.is_empty() {
            let names = shared
                .iter()
                .map(|other| format!("{} ({})", other.title, other.owner))
                .collect::<Vec<_>>()
                .join(", ");
            Some(
                div()
                    .flex()
                    .flex_col()
                    .when(row.override_.is_some(), |this| this.child("Modified"))
                    .when(!shared.is_empty(), |this| {
                        this.child(
                            div()
                                .text_color(c.text_subtle)
                                .child(format!("Also used by {names}")),
                        )
                    })
                    .into_any_element(),
            )
        } else {
            None
        };
        let mut trailing = Vec::new();
        if listening {
            let focused = self.focus.is_focused(window);
            trailing.push(
                div()
                    .id(SharedString::from(format!("shortcut-capture-{key}")))
                    .track_focus(&self.focus)
                    .flex()
                    .items_center()
                    .w(rems(13.5))
                    .max_w_full()
                    .min_h(rems(2.))
                    .px(rems(1.))
                    .rounded(radius::CONTROL)
                    .border_1()
                    .border_color(if focused {
                        c.interaction(Surface::Panel).border
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .bg(c.surface_inset)
                    .text_color(c.text_standard)
                    .text_body_sm()
                    .child("Press a shortcut…")
                    .into_any_element(),
            );
        } else if let Some(primary) = row.effective.first() {
            trailing.push(Kbd::new(primary.text(APPLE)).into_any_element());
        }
        if !row.read_only {
            let (icon, tooltip) = if listening {
                (IconName::X, "Cancel")
            } else {
                (IconName::PencilSimple, "Change shortcut")
            };
            trailing.push(
                IconButton::new(SharedString::from(format!("shortcut-edit-{key}")), icon)
                    .size(IconButtonSize::Sm)
                    .tooltip(tooltip)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if listening {
                            this.cancel(key, cx);
                            window.blur(cx);
                        } else {
                            this.start(key, window, cx);
                        }
                    }))
                    .into_any_element(),
            );
        }
        if row.override_.is_some() && !listening {
            trailing.push(
                button(SharedString::from(format!("shortcut-reset-{key}")), "Reset")
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| this.reset(key, cx)))
                    .into_any_element(),
            );
        }
        let notice = self
            .notice
            .as_ref()
            .filter(|notice| notice.key == key)
            .map(|notice| {
                div()
                    .text_body_sm()
                    .text_color(c.text_standard)
                    .child(notice.message.clone())
            });
        div()
            .child(preference_row(None, row.title, subtitle, trailing, cx))
            .children(notice)
            .into_any_element()
    }
}

impl Render for ShortcutSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let overrides = Shortcuts::overrides(cx);
        let modified = !overrides.is_empty();
        let rows = shortcuts::rows(overrides, DESKTOP);
        let error = Shortcuts::error(cx);
        // Rows come grouped by owner: Buzz, then each plugin.
        let mut groups: Vec<(&'static str, Vec<AnyElement>)> = Vec::new();
        for row in &rows {
            let item = self.row(row, &rows, window, cx);
            match groups.last_mut() {
                Some((owner, items)) if *owner == row.owner => items.push(item),
                _ => groups.push((row.owner, vec![item])),
            }
        }
        section(vec![
            header("Shortcuts", None, cx).into_any_element(),
            div()
                .flex()
                .flex_col()
                .gap(rems(1.25))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(2.))
                        .children(groups.into_iter().map(|(owner, items)| {
                            div()
                                .child(inline_header(owner, None, Vec::new(), cx))
                                .child(rows_group(items, cx))
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(rems(0.75))
                        .child(
                            button("shortcuts-reset-all", "Reset all shortcuts")
                                .disabled(!modified)
                                .on_click(cx.listener(|this, _, _, cx| this.reset_all(cx))),
                        ),
                )
                .when_some(error, |this, error| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(0.75))
                            .text_body_sm()
                            .text_color(c.text_standard)
                            .child(error)
                            .child(
                                div().flex().child(
                                    button("shortcuts-retry", "Retry saving shortcuts")
                                        .on_click(|_, _, cx| Shortcuts::retry(cx)),
                                ),
                            ),
                    )
                })
                .into_any_element(),
        ])
    }
}
