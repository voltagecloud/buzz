//! Settings → Notifications (§5b.5 item 3, buzz-app's
//! `NotificationSettings`): the viewer's notification choices, which
//! apply to every community, and alert sounds. Banners are always silent;
//! the app plays the chosen sound itself.

use super::{SettingsPage, header, inline_header, preference_row, rows_group, section};
use crate::notifications::{Notifications, Preferences, ServiceStatus};
use crate::sounds::{self, Sound};
use crate::ui::overlay::{self, Align};
use crate::ui::toasts;
use buzz_relay_client::Category;
use buzz_ui::{
    ActiveTheme, Button, ButtonShape, ButtonSize, IconName, Menu, MenuItem, Switch, TypeStyled,
    tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// Applies one switch to the choices.
type Apply = Box<dyn Fn(&mut Preferences, bool)>;

/// Sound rows in the reference order: direct messages, mentions, threads.
const SOUND_ROWS: [Category; 3] = [Category::Direct, Category::Mention, Category::Thread];
/// Category switches in the order the categories register (`messages.ts`).
const CATEGORY_ROWS: [(Category, &str); 3] = [
    (Category::Mention, "Mentions"),
    (Category::Direct, "Direct messages"),
    (Category::Thread, "Thread replies"),
];

fn sound_label(category: Category) -> &'static str {
    match category {
        Category::Direct => "Direct messages",
        Category::Mention => "@Mentions",
        Category::Thread => "Thread replies",
    }
}

fn option_label(sound: Sound, category: Category) -> String {
    if sound == Sound::Silent {
        "Silent".into()
    } else if sound == Sound::recommended(category) {
        format!("{} rec.", sound.name())
    } else {
        sound.name().into()
    }
}

/// The status line under Desktop alerts (`permissionStatus`). On Linux
/// the desktop manages permission, so only a missing service shows.
fn service_status(service: ServiceStatus) -> Option<&'static str> {
    (service == ServiceStatus::Unavailable).then_some("This build can’t show system notifications.")
}

/// A switch handler that applies a change to the viewer's choices.
fn change(
    apply: impl Fn(&mut Preferences, bool) + 'static,
) -> impl Fn(bool, &mut Window, &mut App) {
    let apply = Rc::new(apply);
    move |value, _, cx| {
        let mut next = Notifications::preferences(cx);
        apply(&mut next, value);
        if !next.enabled || !next.sound {
            sounds::stop_preview();
        }
        Notifications::update(next, cx);
    }
}

impl SettingsPage {
    pub(super) fn notifications_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let state = cx.global::<Notifications>();
        let preferences = state.preferences.clone();
        let status = service_status(state.service);
        let preferences_error = state.preferences_error;
        let delivery_error = state.error.clone();
        let switch_row = |id: SharedString,
                          title: &'static str,
                          description: Option<&'static str>,
                          checked: bool,
                          apply: Apply,
                          cx: &App| {
            preference_row(
                None,
                title,
                description.map(IntoElement::into_any_element),
                vec![
                    Switch::new(id, checked)
                        .on_change(change(move |p, v| apply(p, v)))
                        .into_any_element(),
                ],
                cx,
            )
            .into_any_element()
        };
        // Category switches stay live with Desktop alerts off.
        let mut categories: Vec<AnyElement> = CATEGORY_ROWS
            .into_iter()
            .map(|(category, label)| {
                switch_row(
                    format!("notify-{category:?}").into(),
                    label,
                    None,
                    preferences.category(category),
                    Box::new(move |p, v| p.set_category(category, v)),
                    cx,
                )
            })
            .collect();
        // Plugin categories follow, such as Reminders.
        for spec in crate::notifications::categories::all(cx) {
            categories.push(switch_row(
                format!("notify-{}", spec.key).into(),
                spec.label,
                None,
                preferences.categories.get(spec.key) != Some(&false),
                Box::new(move |p, v| {
                    p.categories.insert(spec.key.to_owned(), v);
                }),
                cx,
            ));
        }
        let enabled = preferences.enabled;
        let mut delivery = vec![switch_row(
            "notify-desktop".into(),
            "Desktop alerts",
            None,
            enabled,
            Box::new(|p, v| p.enabled = v),
            cx,
        )];
        if enabled && let Some(status) = status {
            delivery.push(
                div()
                    .my(rems(0.75))
                    .p(rems(0.75))
                    .rounded(radius::CONTROL)
                    .bg(c.affordance_warning)
                    .text_color(c.text_warning)
                    .text_body_sm()
                    .child(status)
                    .into_any_element(),
            );
        }
        if enabled {
            delivery.push(switch_row(
                "notify-while-viewing".into(),
                "Notify while viewing",
                Some("Include direct messages in the conversation you’re viewing."),
                preferences.notify_while_viewing,
                Box::new(|p, v| p.notify_while_viewing = v),
                cx,
            ));
            delivery.push(switch_row(
                "notify-sound".into(),
                "Sound",
                Some("Choose a sound to preview it."),
                preferences.sound,
                Box::new(|p, v| p.sound = v),
                cx,
            ));
            if preferences.sound {
                for category in SOUND_ROWS {
                    delivery.push(
                        self.sound_row(category, &preferences, window, cx)
                            .into_any_element(),
                    );
                }
            }
        }
        let mut cards = Vec::new();
        if let Some(description) = preferences_error {
            let notice = toasts::Notice {
                dismissible: false,
                ..toasts::Notice::error(
                    "notification-preferences",
                    "Notification choices weren’t saved",
                )
            }
            .description(description)
            .action(toasts::Action::new("Retry saving choices", |_, cx| {
                Notifications::update(Notifications::preferences(cx), cx)
            }))
            .action(toasts::Action::new("Reload saved choices", |_, cx| {
                Notifications::reload(cx)
            }));
            cards.push(toasts::card(&notice, 0).into_any_element());
        }
        if let Some(description) = delivery_error {
            let notice = toasts::Notice {
                dismissible: false,
                ..toasts::Notice::error(
                    "notification-delivery",
                    "Buzz couldn’t send the notification",
                )
            }
            .description(description);
            cards.push(toasts::card(&notice, 0).into_any_element());
        }
        let mut children = vec![
            header("Notifications", None, cx).into_any_element(),
            div()
                .child(inline_header("Notify me about", None, Vec::new(), cx))
                .child(rows_group(categories, cx))
                .into_any_element(),
            div()
                .child(inline_header("Delivery", None, Vec::new(), cx))
                .child(rows_group(delivery, cx))
                .into_any_element(),
        ];
        children.extend(toasts::stack(window, cards));
        section(children)
    }

    /// `AlertSoundRow`: the category and a compact select of sounds.
    /// Choosing one saves it and previews it.
    fn sound_row(
        &self,
        category: Category,
        preferences: &Preferences,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = *cx.colors();
        let allowed = preferences.category(category);
        let current = preferences.sounds.get(category);
        let open = self.sound_menu == Some(category) && allowed;
        let id = |name: &str| SharedString::from(format!("sound-{name}-{category:?}"));
        let trigger = Button::new(
            id("select"),
            if current == Sound::Silent {
                "Silent"
            } else {
                current.name()
            },
        )
        .compact_select()
        .size(ButtonSize::Sm)
        .shape(ButtonShape::Control)
        .icon_end(IconName::CaretDown)
        .selected(open)
        .disabled(!allowed)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.sound_menu = (this.sound_menu != Some(category)).then_some(category);
            cx.notify();
        }));
        let mut control = div().relative().child(trigger);
        if open {
            let items = Sound::choices(category).into_iter().map(|sound| {
                MenuItem::new(id(sound.name()), option_label(sound, category))
                    .checked(sound == current)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sound_menu = None;
                        let mut next = Notifications::preferences(cx);
                        if sound != next.sounds.get(category) {
                            next.sounds.set(category, sound);
                            Notifications::update(next, cx);
                            if sound == Sound::Silent {
                                sounds::stop_preview();
                            } else {
                                sounds::preview(sound);
                            }
                        }
                        cx.notify();
                    }))
            });
            let page = cx.entity().downgrade();
            control = control.child(overlay::dropdown(
                window,
                Align::End,
                Menu::new().child(
                    div()
                        .id(id("menu"))
                        .max_h(rems(18.))
                        .overflow_y_scroll()
                        .children(items),
                ),
                Rc::new(move |_, cx| {
                    let _ = page.update(cx, |this, cx| {
                        this.sound_menu = None;
                        cx.notify();
                    });
                }),
            ));
        }
        preference_row(
            None,
            sound_label(category),
            None,
            vec![control.into_any_element()],
            cx,
        )
        .when(!allowed, |this| this.text_color(c.text_unavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn options_mark_the_recommended_sound() {
        assert_eq!(option_label(Sound::Silent, Category::Direct), "Silent");
        assert_eq!(option_label(Sound::Unison, Category::Direct), "unison rec.");
        assert_eq!(option_label(Sound::Unison, Category::Mention), "unison");
    }

    #[test]
    fn saved_preferences_without_sounds_get_the_defaults() {
        let old = r#"{"enabled":true,"mention":false,"direct":true,"thread":true}"#;
        let preferences: Preferences = serde_json::from_str(old).unwrap();
        assert!(!preferences.mention && preferences.sound);
        assert_eq!(preferences.sounds.get(Category::Thread), Sound::Flutter);
        assert_eq!(
            preferences.sound_for(Category::Direct),
            Some(Sound::Flutter)
        );
        assert_eq!(preferences.sound_for(Category::Mention), None);
        let silent = Preferences {
            sound: false,
            ..preferences.clone()
        };
        assert_eq!(silent.sound_for(Category::Direct), None);
        let mut chosen = preferences;
        chosen.sounds.set(Category::Direct, Sound::Silent);
        assert_eq!(chosen.sound_for(Category::Direct), None);
        let json = serde_json::to_string(&chosen).unwrap();
        assert!(
            json.contains(r#""sounds":{"mention":"flutter","direct":"silent","thread":"flutter"}"#)
        );
    }
}
