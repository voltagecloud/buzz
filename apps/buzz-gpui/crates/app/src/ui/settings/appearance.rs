//! Settings → Appearance (§1.1, §5b.5 item 2): color mode thumbnails and
//! the interface size stepper, and this port's system tray choices.

use super::{button, header, inline_header, note, preference_row, rows_group, section};
use crate::appearance::{self as saved, AppearanceErrors};
use crate::main_window;
use crate::ui::toasts;
use buzz_ui::{
    ActiveTheme, Appearance, AppearancePreference, ButtonSize, Colors, INTERFACE_SCALE_MAX,
    INTERFACE_SCALE_MIN, IconButton, IconButtonSize, IconName, Switch, Theme, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

pub(super) fn render(window: &Window, cx: &App) -> AnyElement {
    let theme = Theme::base(cx);
    let preference = theme.preference();
    let scale = theme.interface_scale();
    let picker = div().flex().gap(rems(0.75)).children(
        [
            (AppearancePreference::System, "System"),
            (AppearancePreference::Light, "Light"),
            (AppearancePreference::Dark, "Dark"),
        ]
        .into_iter()
        .map(|(mode, label)| thumbnail(mode, label, preference == mode, cx)),
    );
    let percent = (scale * 100.).round() as i32;
    let size = div()
        .flex()
        .items_center()
        .gap(rems(0.75))
        .child(
            button("interface-smaller", "−")
                .size(ButtonSize::Sm)
                .disabled(scale <= INTERFACE_SCALE_MIN + 0.01)
                .tooltip("Decrease interface size")
                .on_click(|_, _, cx| Theme::decrease_interface_scale(cx)),
        )
        .child(
            div()
                .min_w(rems(2.5))
                .text_center()
                .text_body_sm()
                .child(format!("{percent}%")),
        )
        .child(
            button("interface-larger", "+")
                .size(ButtonSize::Sm)
                .disabled(scale >= INTERFACE_SCALE_MAX - 0.01)
                .tooltip("Increase interface size")
                .on_click(|_, _, cx| Theme::increase_interface_scale(cx)),
        )
        .when(percent != 100, |this| {
            this.child(
                IconButton::new("interface-reset", IconName::ArrowCounterClockwise)
                    .size(IconButtonSize::Sm)
                    .tooltip("Reset interface size")
                    .on_click(|_, _, cx| Theme::reset_interface_scale(cx)),
            )
        });
    let mut children = vec![
        header("Appearance", None, cx).into_any_element(),
        rows_group(
            vec![
                preference_row(
                    None,
                    "Color mode",
                    None,
                    vec![picker.into_any_element()],
                    cx,
                )
                .into_any_element(),
                preference_row(
                    None,
                    "Interface size",
                    None,
                    vec![size.into_any_element()],
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        )
        .into_any_element(),
        tray(cx),
    ];
    children.extend(toasts::stack(window, save_errors(cx)));
    section(children)
}

/// "Interface size wasn’t saved" and "Appearance wasn’t saved", each with
/// its retry (`AppearanceSettings.tsx`).
fn save_errors(cx: &App) -> Vec<AnyElement> {
    let Some(errors) = cx.try_global::<AppearanceErrors>() else {
        return Vec::new();
    };
    let mut cards = Vec::new();
    if let Some(description) = errors.scale {
        let notice = toasts::Notice {
            dismissible: false,
            ..toasts::Notice::error("interface-size", "Interface size wasn’t saved")
        }
        .description(description)
        .action(toasts::Action::new(
            "Retry saving interface size",
            |_, cx| saved::save_scale(cx),
        ));
        cards.push(toasts::card(&notice, 0).into_any_element());
    }
    if let Some(description) = errors.appearance {
        let notice = toasts::Notice {
            dismissible: false,
            ..toasts::Notice::error("appearance", "Appearance wasn’t saved")
        }
        .description(description)
        .action(toasts::Action::new("Retry saving appearance", |_, cx| {
            saved::save_appearance(cx)
        }));
        cards.push(toasts::card(&notice, 0).into_any_element());
    }
    cards
}

/// The tray icon and whether the window hides to it.
fn tray(cx: &App) -> AnyElement {
    let preferences = main_window::preferences(cx);
    let toggle = |id: &'static str,
                  title: &'static str,
                  description: &'static str,
                  checked: bool,
                  enabled: bool,
                  apply: fn(&mut main_window::Preferences, bool)| {
        preference_row(
            None,
            title,
            Some(description.into_any_element()),
            vec![
                Switch::new(id, checked)
                    .disabled(!enabled)
                    .on_change(move |value, window, cx| {
                        let mut next = main_window::preferences(cx);
                        apply(&mut next, value);
                        main_window::set_preferences(next, cx);
                        window.refresh();
                    })
                    .into_any_element(),
            ],
            cx,
        )
        .into_any_element()
    };
    let shown = preferences.show_icon;
    let rows = vec![
        toggle(
            "tray-icon",
            "Show Buzz in the system tray",
            "The icon opens Buzz and shows when mentions or direct messages are unread.",
            shown,
            true,
            |p, v| p.show_icon = v,
        ),
        toggle(
            "tray-minimize",
            "Minimize to the tray",
            "The minimize button hides the window instead.",
            preferences.minimize_to_tray,
            shown,
            |p, v| p.minimize_to_tray = v,
        ),
        toggle(
            "tray-close",
            "Close to the tray",
            "Closing the window keeps Buzz running, so notifications keep arriving.",
            preferences.close_to_tray,
            shown,
            |p, v| p.close_to_tray = v,
        ),
    ];
    // Without a panel showing the icon, hiding to it would strand the app.
    let unseen = (shown && !main_window::tray_shown(cx)).then(|| {
        note(
            "No panel is showing the icon, so the window doesn't hide to it. On GNOME, \
             turn on the AppIndicator extension.",
            cx,
        )
    });
    div()
        .flex()
        .flex_col()
        .gap(rems(0.5))
        .child(inline_header("System tray", None, Vec::new(), cx))
        .child(rows_group(rows, cx))
        .children(unseen)
        .into_any_element()
}

/// One `ThemePicker` radio: an 80×56 preview, the system one split down
/// the middle, outlined 1px (2px focus color when chosen) 2px outside.
fn thumbnail(
    mode: AppearancePreference,
    label: &'static str,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let c = cx.colors();
    let unit = |value: f32| rems(value / 16.);
    let previews: Vec<AnyElement> = match mode {
        AppearancePreference::Light => vec![preview(Appearance::Light, 0., 80.)],
        AppearancePreference::Dark => vec![preview(Appearance::Dark, 0., 80.)],
        AppearancePreference::System => vec![
            preview(Appearance::Light, 0., 40.),
            preview(Appearance::Dark, 40., 40.),
        ],
    };
    let (ring, width) = if selected {
        (c.border_focus, 2.)
    } else {
        (c.border_standard, 1.)
    };
    let hover = c.border_prominent;
    div()
        .id(SharedString::from(format!("theme-{label}")))
        .flex_none()
        .p(unit(2. - (width - 1.)))
        .border(px(width))
        .border_color(ring)
        .rounded(unit(8. + 3.))
        .cursor_pointer()
        .when(!selected, |this| this.hover(move |s| s.border_color(hover)))
        .tooltip(buzz_ui::Tooltip::text(label))
        .on_click(move |_, _, cx| Theme::set_preference(mode, cx))
        .child(
            div()
                .relative()
                .w(unit(80.))
                .h(unit(56.))
                .rounded(unit(8.))
                .overflow_hidden()
                .children(previews),
        )
}

/// The 80×56 preview drawing (`ThemePicker.tsx`), showing `width` units
/// of it from `left`.
fn preview(appearance: Appearance, left: f32, width: f32) -> AnyElement {
    let c: Colors = Theme::colors_for(appearance);
    let unit = |value: f32| rems(value / 16.);
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        div()
            .absolute()
            .left(unit(x))
            .top(unit(y))
            .w(unit(w))
            .h(unit(h))
    };
    // Stroked paths are 2 wide with round caps.
    let line = |x: f32, y: f32, length: f32| {
        rect(x - 1., y - 1., length + 2., 2.)
            .rounded_full()
            .bg(c.text_metadata)
    };
    div()
        .absolute()
        .left(unit(left))
        .top_0()
        .w(unit(width))
        .h(unit(56.))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .left(unit(-left))
                .top_0()
                .w(unit(80.))
                .h(unit(56.))
                .bg(c.surface_base)
                .child(rect(5., 5., 70., 46.).rounded(unit(5.)).bg(c.surface_panel))
                .child(rect(22.5, 5., 1., 46.).bg(c.border_standard))
                .child(line(10., 13., 7.))
                .child(line(10., 18., 5.))
                .child(line(29., 17., 30.))
                .child(line(29., 22., 24.))
                .child(line(29., 27., 32.))
                .child(
                    rect(45., 10., 23., 3.)
                        .rounded_full()
                        .bg(c.affordance_accent_prominent),
                )
                .child(
                    rect(49., 32., 19., 3.)
                        .rounded_full()
                        .bg(c.affordance_accent_prominent),
                )
                .child(
                    rect(29., 41., 39., 6.)
                        .rounded_full()
                        .border_1()
                        .border_color(c.border_standard),
                ),
        )
        .into_any_element()
}
