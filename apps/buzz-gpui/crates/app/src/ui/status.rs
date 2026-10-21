//! The "Set a status" dialog (`StatusEditor.tsx`, relay.md §5j): text, an
//! emoji, and how long the status lasts. Also how statuses are shown
//! (`StatusEmoji.tsx`, `StatusDisplay.tsx`).

use crate::community::Community;
use crate::ui::emoji_picker::{self, EmojiPicker, Picked};
use crate::ui::media;
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::user_status::TEXT_LIMIT;
use buzz_relay_client::{Session, StatusInput, UserStatus};
use buzz_ui::{
    ActiveTheme, Button, Dialog, Icon, IconButton, IconButtonSize, IconName, Menu, MenuItem,
    NavigationItem, NavigationItemVariant, Popover, PopoverPadding, PopoverWidth, Select, Surface,
    TextField, TypeStyled,
    tokens::{radius, size},
};
use chrono::{
    DateTime, Datelike, Duration as Days, Local, Months, NaiveDate, NaiveTime, TimeZone, Timelike,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;
use std::sync::Arc;

/// `status-duration.ts` presets. The preset is not saved; editing infers
/// it from the saved expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Duration {
    Hour,
    EightHours,
    Today,
    Week,
    Custom,
    Never,
}

const DURATIONS: [Duration; 6] = [
    Duration::Hour,
    Duration::EightHours,
    Duration::Today,
    Duration::Week,
    Duration::Custom,
    Duration::Never,
];

impl Duration {
    fn label(self) -> &'static str {
        match self {
            Self::Hour => "1 hour",
            Self::EightHours => "8 hours",
            Self::Today => "Today",
            Self::Week => "This week",
            Self::Custom => "Custom",
            Self::Never => "Don’t clear",
        }
    }
}

/// When a preset ends, counted from `now`: local midnight tonight, or the
/// next local Monday at midnight for "This week".
pub fn deadline(duration: Duration, now: DateTime<Local>) -> Option<i64> {
    let midnight = |days: i64| {
        let date = now.date_naive() + Days::days(days);
        Local
            .from_local_datetime(&date.and_time(NaiveTime::MIN))
            .earliest()
            .map(|at| at.timestamp())
    };
    match duration {
        Duration::Hour => Some(now.timestamp() + 3600),
        Duration::EightHours => Some(now.timestamp() + 28_800),
        Duration::Today => midnight(1),
        Duration::Week => {
            // Days until next Monday; a full week on Mondays.
            let weekday = i64::from(now.weekday().num_days_from_sunday());
            let days = match (8 - weekday) % 7 {
                0 => 7,
                days => days,
            };
            midnight(days)
        }
        Duration::Custom | Duration::Never => None,
    }
}

/// The preset whose deadline, counted from `updated_at`, lands within two
/// minutes of `expires_at`; otherwise Custom.
pub fn infer(expires_at: Option<u64>, updated_at: u64) -> Duration {
    let Some(expires_at) = expires_at else {
        return Duration::Never;
    };
    let Some(from) = Local.timestamp_opt(updated_at as i64, 0).single() else {
        return Duration::Custom;
    };
    DURATIONS
        .into_iter()
        .filter_map(|duration| {
            deadline(duration, from).map(|at| (duration, (at - expires_at as i64).abs()))
        })
        .filter(|(_, distance)| *distance < 121)
        .min_by_key(|(_, distance)| *distance)
        .map_or(Duration::Custom, |(duration, _)| duration)
}

const QUICK: [(&str, &str); 5] = [
    ("🗣️", "In a meeting"),
    ("🚌", "Commuting"),
    ("🤒", "Out sick"),
    ("🏖️", "Vacationing"),
    ("🏠", "Working remotely"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Emoji,
    Duration,
    Date,
    Time,
}

/// The dialog finished: `Some` with what was saved (`None` inside when
/// cleared), or `None` when cancelled.
pub struct Closed(pub Option<Option<UserStatus>>);

pub struct StatusEditor {
    session: Arc<Session>,
    /// For custom emoji images.
    community: Entity<Community>,
    /// The composer's emoji picker, custom emoji included.
    emoji_picker: Entity<EmojiPicker>,
    current: Option<UserStatus>,
    text: Entity<InputState>,
    emoji: String,
    duration: Duration,
    duration_changed: bool,
    custom: DateTime<Local>,
    /// The first day of the month the calendar shows.
    month: NaiveDate,
    picker: Option<Picker>,
    busy: bool,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Closed> for StatusEditor {}

impl StatusEditor {
    pub fn new(
        community: Entity<Community>,
        current: Option<UserStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| {
            // `maxLength`, which counts UTF-16 units.
            let mut state = InputState::new(window, cx)
                .placeholder("What’s your status?")
                .validate(|text, _| within_limit(text));
            if let Some(current) = &current {
                state = state.default_value(current.text.clone());
            }
            state
        });
        text.update(cx, |state, cx| state.focus(window, cx));
        let session = community.read(cx).session.clone();
        let emoji_picker = cx.new(|cx| EmojiPicker::new(community.clone(), window, cx));
        let _subscriptions = vec![
            cx.subscribe(&text, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.submit(false, cx),
                _ => {}
            }),
            cx.subscribe(&emoji_picker, |this, _, Picked(pick), cx| {
                this.emoji = pick.text();
                this.set_picker(None, cx);
            }),
            cx.subscribe(&emoji_picker, |this, _, _: &emoji_picker::Dismissed, cx| {
                this.set_picker(None, cx)
            }),
        ];
        let now = Local::now();
        let duration = current.as_ref().map_or(Duration::Today, |status| {
            infer(status.expires_at, status.updated_at)
        });
        let custom = current
            .as_ref()
            .and_then(|status| status.expires_at)
            .and_then(|at| Local.timestamp_opt(at as i64, 0).single())
            .or_else(|| {
                deadline(Duration::Today, now).and_then(|at| Local.timestamp_opt(at, 0).single())
            })
            .unwrap_or(now);
        Self {
            session,
            community,
            emoji_picker,
            emoji: current
                .as_ref()
                .map(|status| status.emoji.clone())
                .unwrap_or_default(),
            current,
            text,
            duration,
            duration_changed: false,
            month: first_of_month(custom.date_naive()),
            custom,
            picker: None,
            busy: false,
            error: None,
            _subscriptions,
        }
    }

    fn text(&self, cx: &App) -> String {
        self.text.read(cx).value().trim().to_string()
    }

    /// The chosen emoji, or 💬 for a text-only status.
    fn effective_emoji(&self, cx: &App) -> String {
        if !self.emoji.is_empty() {
            self.emoji.clone()
        } else if !self.text(cx).is_empty() {
            "💬".into()
        } else {
            String::new()
        }
    }

    fn set_picker(&mut self, picker: Option<Picker>, cx: &mut Context<Self>) {
        if picker == Some(Picker::Date) {
            self.month = first_of_month(self.custom.date_naive());
        }
        self.picker = picker;
        cx.notify();
    }

    fn set_duration(&mut self, duration: Duration, cx: &mut Context<Self>) {
        self.duration = duration;
        self.duration_changed = true;
        self.picker = None;
        self.error = None;
        cx.notify();
    }

    fn set_custom(&mut self, at: DateTime<Local>, cx: &mut Context<Self>) {
        self.custom = at;
        self.duration_changed = true;
        self.picker = None;
        self.error = None;
        cx.notify();
    }

    fn submit(&mut self, clear: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let text = self.text(cx);
        let emoji = self.effective_emoji(cx);
        if !clear && text.is_empty() && self.emoji.is_empty() {
            return;
        }
        let now = Local::now().timestamp();
        let current_expiry = self.current.as_ref().and_then(|status| {
            let live = status.expires_at.is_none_or(|at| at as i64 > now);
            (!self.duration_changed && live).then_some(status.expires_at)
        });
        let expires_at = if clear || self.duration == Duration::Never {
            None
        } else if let Some(expiry) = current_expiry {
            expiry.map(|at| at as i64)
        } else if self.duration == Duration::Custom {
            Some(self.custom.timestamp())
        } else {
            deadline(self.duration, Local::now())
        };
        if !clear && expires_at.is_some_and(|at| at <= now) {
            self.error = Some("Choose a date and time in the future.".into());
            cx.notify();
            return;
        }
        let input = if clear {
            StatusInput::default()
        } else {
            StatusInput {
                text,
                emoji,
                expires_at: expires_at.map(|at| at as u64),
            }
        };
        self.busy = true;
        self.error = None;
        let session = self.session.clone();
        cx.spawn(async move |this, cx| {
            let result = session.set_status(input.clone()).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        let saved =
                            (!input.text.is_empty() || !input.emoji.is_empty()).then(|| {
                                UserStatus {
                                    text: input.text,
                                    emoji: input.emoji,
                                    updated_at: Local::now().timestamp() as u64,
                                    expires_at: input.expires_at,
                                }
                            });
                        cx.emit(Closed(Some(saved)));
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn dismiss(cx: &mut Context<Self>) -> Dismiss {
        let editor = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = editor.update(cx, |this, cx| this.set_picker(None, cx));
        })
    }

    fn label(text: &'static str, cx: &App) -> Div {
        div()
            .text_label_sm()
            .text_color(cx.colors().text_subtle)
            .child(text)
    }

    /// A `FieldButton` that shows `menu` below it while `picker` is open.
    fn field_button(
        &self,
        id: &'static str,
        value: String,
        picker: Picker,
        menu: fn(&Self, &mut Context<Self>) -> AnyElement,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.picker == Some(picker);
        let mut wrapper = div().relative().w_full().child(
            Select::new(id)
                .value(value)
                .open(open)
                .surface(Surface::Floating)
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _, _, cx| this.set_picker(Some(picker), cx))),
        );
        if open {
            wrapper = wrapper.child(overlay::dropdown(
                window,
                Align::End,
                menu(self, cx),
                Self::dismiss(cx),
            ));
        }
        wrapper
    }

    fn duration_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let keep_never = self
            .current
            .as_ref()
            .is_some_and(|status| status.expires_at.is_none());
        Menu::new()
            .children(
                DURATIONS
                    .into_iter()
                    .filter(|duration| *duration != Duration::Never || keep_never)
                    .map(|duration| {
                        MenuItem::new(SharedString::from(duration.label()), duration.label())
                            .checked(self.duration == duration)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.set_duration(duration, cx)),
                            )
                    }),
            )
            .into_any_element()
    }

    /// The custom expiry's date: a month calendar (buzz-app's `Calendar`,
    /// react-day-picker), days before today disabled. Picking a day keeps
    /// the chosen time.
    fn date_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let today = Local::now().date_naive();
        let chosen = self.custom.date_naive();
        let time = self.custom.time();
        let month = self.month;
        let control = size::CONTROL_SM;
        let nav = |id: &'static str, icon: IconName, to: NaiveDate, cx: &mut Context<Self>| {
            let hover = c.affordance_subtle_hover;
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(control)
                .rounded(radius::CONTROL)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(Icon::new(icon).size(16.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.month = to;
                    cx.notify();
                }))
        };
        let caption = div()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .h(control)
            .mb(rems(0.5))
            .text_label_sm()
            .child(month.format("%B %Y").to_string())
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_between()
                    .child(nav(
                        "calendar-previous",
                        IconName::CaretLeft,
                        month - Months::new(1),
                        cx,
                    ))
                    .child(nav(
                        "calendar-next",
                        IconName::CaretRight,
                        month + Months::new(1),
                        cx,
                    )),
            );
        let cell = || div().flex().items_center().justify_center().p(rems(0.125));
        let weekdays = div()
            .flex()
            .children(["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"].map(|name| {
                cell().child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(control)
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child(name),
                )
            }));
        let days: Vec<NaiveDate> = month_days(month);
        let weeks = days.chunks(7).map(|week| {
            div().flex().children(week.iter().map(|&date| {
                let disabled = date < today;
                let selected = date == chosen;
                let outside = date.month() != month.month();
                let at = Local.from_local_datetime(&date.and_time(time)).earliest();
                let hover = c.affordance_subtle_hover;
                let day = div()
                    .id(SharedString::from(format!("day-{date}")))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(control)
                    .rounded(radius::CONTROL)
                    .text_body_sm()
                    .map(|this| {
                        if selected {
                            this.bg(c.affordance_prominent).text_color(c.text_inverse)
                        } else if disabled {
                            this.text_color(c.text_unavailable)
                        } else if outside {
                            this.text_color(c.text_subtle)
                        } else {
                            this
                        }
                    })
                    .when(date == today && !selected, |this| {
                        this.bg(c.affordance_subtle)
                    })
                    .when(!disabled, |this| {
                        this.cursor_pointer()
                            .when(!selected, |this| this.hover(move |s| s.bg(hover)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(at) = at {
                                    this.set_custom(at, cx);
                                }
                            }))
                    })
                    .child(date.day().to_string());
                cell().child(day)
            }))
        });
        Popover::new()
            .width(PopoverWidth::Auto)
            .padding(PopoverPadding::None)
            .child(
                div()
                    .id("status-calendar")
                    .p(rems(0.75))
                    .text_body_sm()
                    .child(caption)
                    .child(weekdays)
                    .children(weeks),
            )
            .into_any_element()
    }

    fn time_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let date = self.custom.date_naive();
        let chosen = (self.custom.hour(), self.custom.minute());
        let items = (0..48).map(|slot| {
            let (hour, minute) = (slot / 2, (slot % 2) * 30);
            let at = NaiveTime::from_hms_opt(hour, minute, 0)
                .and_then(|time| Local.from_local_datetime(&date.and_time(time)).earliest());
            MenuItem::new(
                SharedString::from(format!("time-{slot}")),
                time_label(hour, minute),
            )
            .checked(chosen == (hour, minute))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(at) = at {
                    this.set_custom(at, cx);
                }
            }))
        });
        Menu::new()
            .child(
                div()
                    .id("status-times")
                    .max_h(rems(18.))
                    .overflow_y_scroll()
                    .children(items),
            )
            .into_any_element()
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

/// The days a month's calendar shows: whole weeks from Sunday, with the
/// neighbouring months' days filling the first and last (react-day-picker's
/// `showOutsideDays`).
fn month_days(month: NaiveDate) -> Vec<NaiveDate> {
    let first = first_of_month(month);
    let last = first + Months::new(1) - Days::days(1);
    let start = first - Days::days(i64::from(first.weekday().num_days_from_sunday()));
    let end = last + Days::days(6 - i64::from(last.weekday().num_days_from_sunday()));
    start.iter_days().take_while(|day| *day <= end).collect()
}

/// Whether status text fits the input (`STATUS_TEXT_LIMIT`, UTF-16 units).
fn within_limit(text: &str) -> bool {
    text.encode_utf16().count() <= TEXT_LIMIT
}

/// The code of a custom `:shortcode:` emoji (`/^:([^:\s]+):$/`).
fn shortcode(value: &str) -> Option<&str> {
    value
        .strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .filter(|code| !code.is_empty() && !code.contains(':'))
        .filter(|code| !code.chars().any(char::is_whitespace))
}

/// A status emoji (`StatusEmoji.tsx`): a custom `:shortcode:` as its
/// catalog image, matched case-insensitively; anything else, an unknown
/// shortcode or an image that fails to load, as text. Nothing for an
/// empty value.
pub fn emoji(
    value: &str,
    size: Pixels,
    community: &Entity<Community>,
    cx: &App,
) -> Option<AnyElement> {
    if value.is_empty() {
        return None;
    }
    let code = shortcode(value);
    let text = {
        let value = SharedString::from(value.to_string());
        let custom = code.is_some();
        move || {
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_size(size)
                .line_height(size)
                // An unresolved shortcode can be long.
                .when(custom, |this| this.flex_shrink(1.).min_w_0().truncate())
                .child(value.clone())
                .into_any_element()
        }
    };
    let source = code.and_then(|code| {
        let community = community.read(cx);
        community
            .snapshot
            .emoji
            .entries
            .iter()
            .find(|entry| entry.shortcode.eq_ignore_ascii_case(code))
            .map(|entry| media::image(&entry.url, community.session.media()))
    });
    Some(match source {
        Some(source) => img(source)
            .flex_none()
            .size(size)
            .object_fit(ObjectFit::Contain)
            .with_fallback(text)
            .into_any_element(),
        None => text(),
    })
}

/// Someone's status in full (`StatusDisplay`): emoji and text in subtle
/// body-sm. Nothing without a status.
pub fn display(
    status: Option<&UserStatus>,
    community: &Entity<Community>,
    cx: &App,
) -> Option<AnyElement> {
    let status = status.filter(|status| !status.emoji.is_empty() || !status.text.is_empty())?;
    Some(
        div()
            .flex()
            .items_center()
            .gap(rems(0.25))
            .min_w_0()
            .max_w_full()
            .text_body_sm()
            .text_color(cx.colors().text_subtle)
            .children(emoji(&status.emoji, px(20.), community, cx))
            .child(div().min_w_0().truncate().child(status.text.clone()))
            .into_any_element(),
    )
}

fn time_label(hour: u32, minute: u32) -> String {
    let twelve = match hour % 12 {
        0 => 12,
        hour => hour,
    };
    let half = if hour < 12 { "AM" } else { "PM" };
    format!("{twelve}:{minute:02} {half}")
}

impl Render for StatusEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let effective = self.effective_emoji(cx);
        let emoji_open = self.picker == Some(Picker::Emoji);
        let emoji_button = div()
            .relative()
            .child(
                IconButton::new("status-emoji", IconName::Smiley)
                    .size(IconButtonSize::Sm)
                    .surface(Surface::Floating)
                    .selected(emoji_open)
                    .disabled(self.busy)
                    .icon_size(22.)
                    .tooltip("Choose a status emoji")
                    .when_some(
                        emoji(&effective, px(20.), &self.community, cx),
                        |this, emoji| this.child(emoji),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !emoji_open {
                            this.emoji_picker.update(cx, |picker, cx| {
                                picker.reset(window, cx);
                                picker.focus(window, cx);
                            });
                        }
                        this.set_picker((!emoji_open).then_some(Picker::Emoji), cx)
                    })),
            )
            .when(emoji_open, |this| {
                this.child(overlay::dropdown(
                    window,
                    Align::Start,
                    emoji_picker::emoji_picker(&self.emoji_picker, {
                        let dismiss = Self::dismiss(cx);
                        move |_, window, cx| dismiss(window, cx)
                    }),
                    Self::dismiss(cx),
                ))
            });
        let message = TextField::new(&self.text)
            .surface(Surface::Floating)
            .disabled(self.busy)
            .leading(emoji_button);

        let duration_label = if self.duration == Duration::Custom {
            "Custom date and time".to_string()
        } else {
            self.duration.label().to_string()
        };
        let mut duration = div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .child(Self::label("Duration", cx))
            .child(self.field_button(
                "status-duration",
                duration_label,
                Picker::Duration,
                Self::duration_menu,
                window,
                cx,
            ));
        if self.duration == Duration::Custom {
            let date = self.custom.format("%b %-d, %Y").to_string();
            let time = time_label(self.custom.hour(), self.custom.minute());
            duration = duration.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(Self::label("Until", cx))
                    .child(
                        div()
                            .flex()
                            .gap(rems(0.5))
                            .child(div().flex_1().min_w_0().child(self.field_button(
                                "status-date",
                                date,
                                Picker::Date,
                                Self::date_menu,
                                window,
                                cx,
                            )))
                            .child(
                                // `minmax(0,1fr) minmax(0,0.8fr)`.
                                div()
                                    .flex_none()
                                    .w(relative(0.8 / 1.8))
                                    .child(self.field_button(
                                        "status-time",
                                        time,
                                        Picker::Time,
                                        Self::time_menu,
                                        window,
                                        cx,
                                    )),
                            ),
                    ),
            );
        }

        let quick = self.current.is_none().then(|| {
            div()
                .flex()
                .flex_col()
                .gap(rems(0.5))
                .child(Self::label("Quick statuses", cx))
                .child(
                    // Option rows, flush with the section's text.
                    div()
                        .flex()
                        .flex_col()
                        .mx(rems(-0.5))
                        .children(QUICK.into_iter().map(|(emoji, label)| {
                            NavigationItem::new(SharedString::from(format!("quick-{label}")), label)
                                .variant(NavigationItemVariant::Option)
                                .surface(Surface::Floating)
                                .leading(div().text_body_lg().child(emoji))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.emoji = emoji.to_string();
                                    this.text.update(cx, |state, cx| {
                                        state.set_value(label, window, cx);
                                        state.focus(window, cx);
                                    });
                                    cx.notify();
                                }))
                        })),
                )
        });

        let can_save = !self.text(cx).is_empty() || !self.emoji.is_empty();
        let mut dialog =
            Dialog::new("status-editor", "Set a status")
                .on_close(cx.listener(|this, _, _, cx| {
                    if !this.busy {
                        cx.emit(Closed(None));
                    }
                }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(1.25))
                        .child(message)
                        .child(duration)
                        .children(quick)
                        .children(self.error.clone().map(|error| {
                            div().text_body_sm().text_color(c.text_danger).child(error)
                        })),
                )
                .footer(
                    Button::new("status-save", "Save status")
                        .prominent()
                        .loading(self.busy)
                        .disabled(self.busy || !can_save)
                        .on_click(cx.listener(|this, _, _, cx| this.submit(false, cx))),
                );
        if self.current.is_some() {
            // A ghost button with danger text.
            let busy = self.busy;
            let hover = c.interaction(Surface::Floating).hover;
            dialog = dialog.footer_leading(
                div()
                    .id("status-clear")
                    .flex()
                    .items_center()
                    .h(rems(2.5))
                    .px(rems(1.5))
                    .rounded(radius::CAPSULE)
                    .text_label_sm()
                    .text_color(if busy {
                        c.text_unavailable
                    } else {
                        c.text_danger
                    })
                    .when(!busy, |this| {
                        this.cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .on_click(cx.listener(|this, _, _, cx| this.submit(true, cx)))
                    })
                    .child("Clear status"),
            );
        }
        // Escape closes an open menu or calendar before the dialog.
        div()
            .capture_action(cx.listener(|this, _: &buzz_ui::Cancel, _, cx| {
                if this.picker.take().is_some() {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn deadlines_follow_the_reference() {
        // Friday 2026-10-09 15:00.
        let now = at(2026, 10, 9, 15, 0);
        assert_eq!(deadline(Duration::Hour, now), Some(now.timestamp() + 3600));
        assert_eq!(
            deadline(Duration::Today, now),
            Some(at(2026, 10, 10, 0, 0).timestamp())
        );
        assert_eq!(
            deadline(Duration::Week, now),
            Some(at(2026, 10, 12, 0, 0).timestamp())
        );
        // On a Monday, "This week" runs to the next Monday.
        let monday = at(2026, 10, 12, 9, 0);
        assert_eq!(
            deadline(Duration::Week, monday),
            Some(at(2026, 10, 19, 0, 0).timestamp())
        );
        // On a Sunday, it ends at midnight.
        let sunday = at(2026, 10, 11, 9, 0);
        assert_eq!(
            deadline(Duration::Week, sunday),
            Some(at(2026, 10, 12, 0, 0).timestamp())
        );
        assert_eq!(deadline(Duration::Never, now), None);
    }

    #[test]
    fn infers_the_preset_from_the_expiry() {
        let updated = at(2026, 10, 9, 10, 0);
        let t = updated.timestamp() as u64;
        assert_eq!(infer(None, t), Duration::Never);
        assert_eq!(infer(Some(t + 3600), t), Duration::Hour);
        assert_eq!(infer(Some(t + 3600 + 100), t), Duration::Hour);
        assert_eq!(
            infer(Some(at(2026, 10, 10, 0, 0).timestamp() as u64), t),
            Duration::Today
        );
        assert_eq!(infer(Some(t + 5000), t), Duration::Custom);
        assert_eq!(time_label(0, 30), "12:30 AM");
        assert_eq!(time_label(13, 0), "1:00 PM");
    }

    #[test]
    fn calendars_show_whole_weeks() {
        let day = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        // October 2026 starts on a Thursday and ends on a Saturday.
        let october = month_days(day(10, 9));
        assert_eq!(october.first(), Some(&day(9, 27)));
        assert_eq!(october.last(), Some(&day(10, 31)));
        assert_eq!(october.len(), 35);
        // February 2026 starts on a Sunday: four whole weeks.
        let february = month_days(day(2, 14));
        assert_eq!(february.first(), Some(&day(2, 1)));
        assert_eq!(february.len(), 28);
        assert_eq!(first_of_month(day(12, 31)), day(12, 1));
    }

    #[test]
    fn caps_text_in_utf16_units() {
        assert!(within_limit(&"x".repeat(100)));
        assert!(!within_limit(&"x".repeat(101)));
        // Each of these takes two UTF-16 units, as `maxLength` counts.
        assert!(within_limit(&"🧪".repeat(50)));
        assert!(!within_limit(&format!("{}x", "🧪".repeat(50))));
    }

    #[test]
    fn recognizes_custom_shortcodes() {
        assert_eq!(shortcode(":party_parrot:"), Some("party_parrot"));
        assert_eq!(shortcode("🎉"), None);
        assert_eq!(shortcode("::"), None);
        assert_eq!(shortcode(":a:b:"), None);
        assert_eq!(shortcode(":a b:"), None);
        assert_eq!(shortcode(":a\u{a0}b:"), None);
        assert_eq!(shortcode(":code"), None);
    }
}
