//! Connection and live-update notices: the relay connection error of
//! `service.ts` as a toast, and `LiveStatus.tsx`'s recovery in the
//! conversation, right above the composer.

use crate::community::Community;
use buzz_relay_client::SessionStatus;
use buzz_ui::{ActiveTheme, Button, ButtonSize, Toast, ToastTone, TypeStyled};
use gpui_kit::*;

/// "Relay connection failed" with Try again, in the toast region.
pub fn toast(community: &Entity<Community>, window: &Window, cx: &App) -> Option<AnyElement> {
    let snapshot = community.read(cx).snapshot.clone();
    // The relay never answered: nothing live, maybe a saved snapshot.
    let SessionStatus::Failed(error) = &snapshot.status else {
        return None;
    };
    let description = if snapshot.cached {
        format!("{error} Showing saved conversations. Reconnect to update saved conversations.")
    } else {
        error.clone()
    };
    let session = community.read(cx).session.clone();
    Some(super::toasts::place(
        window,
        Toast::new("relay-connection", "Relay connection failed")
            .tone(ToastTone::Error)
            .description(description)
            .action(
                Button::new("relay-connection-retry", "Try again")
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, _| session.retry()),
            ),
    ))
}

/// "Live updates need attention" for `channel`, if it does: lasting
/// recovery kept in the conversation so Send stays usable. Retry hands
/// focus to the composer through `on_retry`, as the button goes away.
pub fn recovery(
    community: &Entity<Community>,
    channel: Option<&str>,
    on_retry: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Option<AnyElement> {
    let snapshot = community.read(cx).snapshot.clone();
    if snapshot.status != SessionStatus::Ready || snapshot.cached {
        return None;
    }
    // Startup and bounded automatic quota recovery stay quiet.
    let reason = snapshot.live_status.issue(channel)?;
    let session = community.read(cx).session.clone();
    let c = cx.colors();
    Some(
        div()
            .id("live-status")
            .flex()
            .flex_wrap()
            .items_center()
            .flex_none()
            .gap(rems(0.75))
            .max_h(relative(0.4))
            .overflow_y_scroll()
            .px(rems(1.))
            .py(rems(0.75))
            .border_t_1()
            .border_color(c.border_standard)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow(1.)
                    .flex_basis(rems(16.))
                    .min_w_0()
                    .gap(rems(0.25))
                    .child(
                        div()
                            .text_label_sm()
                            .text_color(c.text_warning)
                            .child("Live updates need attention"),
                    )
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child(format!(
                                "{reason} Only currently accessible messages remain readable."
                            )),
                    ),
            )
            .child(
                Button::new("live-status-retry", "Retry live updates")
                    .size(ButtonSize::Sm)
                    .on_click(move |_, window, cx| {
                        session.retry_live();
                        on_retry(window, cx);
                    }),
            )
            .into_any_element(),
    )
}
