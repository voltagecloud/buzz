//! Fields shared by "Create a channel" and "Edit channel details":
//! `ChannelTextField.tsx` (name and description with a late counter and
//! code-point bounded input), `ChannelDurationField.tsx` (Ongoing or
//! Temporary, in 1 day / 1 week / 2 week steps) and
//! `ChannelPrivacyConfirmation.tsx` with its per-community opt-out.

use super::modal::error;
use crate::community::{AppState, Community};
use buzz_relay_client::lifecycle::{MAX_ABOUT_CHARS, MAX_NAME_CHARS};
use buzz_ui::{
    ActiveTheme, Checkbox, IconButton, IconButtonSize, IconName, Surface, TextArea, TextField,
    TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Text fields

/// Which channel text field: the limits and copy differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    Name,
    Description,
}

impl TextKind {
    pub fn limit(self) -> usize {
        match self {
            Self::Name => MAX_NAME_CHARS,
            Self::Description => MAX_ABOUT_CHARS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Description => "Description",
        }
    }
}

/// `1234` as `1,234`, as `toLocaleString("en-US")` writes it.
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The `count/limit` shown beside the label from 90 % of the limit, and
/// only while the field shows no error.
pub fn counter(count: usize, limit: usize, has_error: bool) -> Option<String> {
    (count * 10 >= limit * 9 && !has_error)
        .then(|| format!("{}/{}", thousands(count), thousands(limit)))
}

/// `boundedInput`: trims only the growth past the limit, at the caret, so
/// typing or pasting in the middle never eats existing text, and text
/// already above the limit stays. Counts code points. Returns the new text
/// and caret (a byte offset) when it trimmed anything.
pub fn bounded_input(
    value: &str,
    caret: usize,
    previous_count: usize,
    limit: usize,
) -> Option<(String, usize)> {
    let ceiling = limit.max(previous_count);
    let excess = value.chars().count().checked_sub(ceiling)?;
    if excess == 0 {
        return None;
    }
    let caret = caret.min(value.len());
    let caret = (0..=caret)
        .rev()
        .find(|at| value.is_char_boundary(*at))
        .unwrap_or(0);
    let (before, after) = value.split_at(caret);
    let keep = before.chars().count().saturating_sub(excess);
    let prefix: String = before.chars().take(keep).collect();
    let at = prefix.len();
    Some((prefix + after, at))
}

/// Keeps an input within a code-point limit as the user edits it.
pub struct Bounded {
    /// Code points before the latest edit: text above the limit that was
    /// already there may stay.
    previous: Rc<Cell<usize>>,
    _subscription: Subscription,
}

/// The single-line and multi-line inputs a [`Bounded`] can keep.
pub trait TextState: EventEmitter<InputEvent> + Sized + 'static {
    fn text(&self) -> String;
    fn caret(&self) -> usize;
    /// Replaces the text as an edit and puts the caret at `caret`.
    fn trim_to(&mut self, text: String, caret: usize, window: &mut Window, cx: &mut Context<Self>);
    /// Replaces the text without an edit.
    fn load(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>);
}

macro_rules! text_state {
    ($state:ty) => {
        impl TextState for $state {
            fn text(&self) -> String {
                self.value().to_string()
            }

            fn caret(&self) -> usize {
                self.cursor()
            }

            fn trim_to(
                &mut self,
                text: String,
                caret: usize,
                window: &mut Window,
                cx: &mut Context<Self>,
            ) {
                self.replace_all(text, window, cx);
                self.set_selected_range(caret..caret, cx);
            }

            fn load(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
                self.set_value(text, window, cx);
            }
        }
    };
}

text_state!(InputState);
text_state!(TextareaState);

impl Bounded {
    pub fn new<S: TextState, V: 'static>(
        state: &Entity<S>,
        limit: usize,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Self {
        let previous = Rc::new(Cell::new(state.read(cx).text().chars().count()));
        let seen = previous.clone();
        let subscription = cx.subscribe_in(
            state,
            window,
            move |_, state, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let (value, caret) = {
                    let state = state.read(cx);
                    (state.text(), state.caret())
                };
                match bounded_input(&value, caret, seen.get(), limit) {
                    Some((text, at)) => {
                        seen.set(text.chars().count());
                        state.update(cx, |state, cx| state.trim_to(text, at, window, cx));
                    }
                    None => seen.set(value.chars().count()),
                }
            },
        );
        Self {
            previous,
            _subscription: subscription,
        }
    }

    /// Replaces the text without an edit, e.g. when a draft loads.
    pub fn set<S: TextState>(
        &self,
        state: &Entity<S>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.previous.set(text.chars().count());
        state.update(cx, |state, cx| state.load(text.to_owned(), window, cx));
    }
}

/// A labelled field: label with the late counter, the control, and the
/// error under it (`Field`).
fn field(
    kind: TextKind,
    count: usize,
    error_text: Option<&'static str>,
    control: AnyElement,
    cx: &App,
) -> Div {
    let c = cx.colors();
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .min_w_0()
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap(space::S3)
                .text_label_sm()
                .text_color(c.text_standard)
                .child(kind.label())
                .children(
                    counter(count, kind.limit(), error_text.is_some())
                        .map(|count| div().text_body_sm().text_color(c.text_subtle).child(count)),
                ),
        )
        .child(control)
        .children(error_text.map(|text| error(text, cx)))
}

/// "Name": one line, `release-notes` placeholder.
pub fn name_field(
    state: &Entity<InputState>,
    error_text: Option<&'static str>,
    disabled: bool,
    cx: &App,
) -> Div {
    let count = state.read(cx).value().chars().count();
    let control = TextField::new(state)
        .surface(Surface::Floating)
        .invalid(error_text.is_some())
        .disabled(disabled)
        .into_any_element();
    field(TextKind::Name, count, error_text, control, cx)
}

/// "Description": three rows, `What this channel is for` placeholder.
pub fn description_field(
    state: &Entity<TextareaState>,
    error_text: Option<&'static str>,
    disabled: bool,
    cx: &App,
) -> Div {
    let count = state.read(cx).value().chars().count();
    let control = TextArea::new(state)
        .surface(Surface::Floating)
        .invalid(error_text.is_some())
        .disabled(disabled)
        .into_any_element();
    field(TextKind::Description, count, error_text, control, cx)
}

// ---------------------------------------------------------------------------
// Duration

/// The temporary durations the steppers move between.
pub const DURATIONS: [u64; 3] = [86_400, 604_800, 1_209_600];

/// The next shorter step, if any.
pub fn shorter(ttl: u64) -> Option<u64> {
    DURATIONS.iter().rev().find(|step| **step < ttl).copied()
}

/// The next longer step, if any.
pub fn longer(ttl: u64) -> Option<u64> {
    DURATIONS.iter().find(|step| **step > ttl).copied()
}

/// `1 day`, `1 week`, `2 weeks`, or the largest whole unit of a custom
/// interval kept from the relay.
pub fn duration_label(ttl: u64) -> String {
    let (count, unit) = if ttl == 1_209_600 {
        (2, "week")
    } else if ttl.is_multiple_of(86_400) {
        (ttl / 86_400, "day")
    } else if ttl.is_multiple_of(3_600) {
        (ttl / 3_600, "hour")
    } else if ttl.is_multiple_of(60) {
        (ttl / 60, "minute")
    } else {
        (ttl, "second")
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

pub type OnDuration = Rc<dyn Fn(Option<u64>, &mut Window, &mut App)>;

/// A card radio (`Radio variant="card"`) on a floating surface.
fn radio_card(
    id: &'static str,
    label: &'static str,
    description: AnyElement,
    checked: bool,
    disabled: bool,
    on_select: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let c = cx.colors();
    let ix = c.interaction(Surface::Floating);
    let (fill, border) = match (disabled, checked) {
        (true, _) => (c.affordance_disabled, ix.border),
        (false, true) => (c.affordance_prominent, c.affordance_prominent),
        (false, false) => (c.surface_panel, ix.border),
    };
    let indicator = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(rems(1.25))
        .rounded(radius::PILL)
        .border_1()
        .border_color(border)
        .bg(fill)
        .when(checked, |this| {
            this.child(div().size(space::S2).rounded(radius::PILL).bg(if disabled {
                c.text_unavailable
            } else {
                c.text_inverse
            }))
        });
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(space::S3)
        .min_h(rems(2.5))
        .p(space::S4)
        .border_1()
        .rounded(radius::CONTROL)
        .map(|this| {
            if checked {
                this.border_color(ix.border).bg(ix.selected)
            } else {
                this.border_color(c.border_standard)
            }
        })
        .when(disabled, |this| this.cursor_not_allowed())
        .when(!disabled, |this| {
            this.cursor_pointer()
                .on_click(move |_, window, cx| on_select(window, cx))
        })
        .child(indicator)
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .text_label_sm()
                        .text_color(if disabled {
                            c.text_unavailable
                        } else {
                            c.text_standard
                        })
                        .child(label),
                )
                .child(
                    div()
                        .text_body_sm()
                        .text_color(if checked {
                            ix.selected_text
                        } else {
                            c.text_subtle
                        })
                        .child(description),
                ),
        )
}

/// "Ongoing" or "Temporary", with steppers beside Temporary while it is
/// chosen. `ttl` is the temporary duration shown and kept while Ongoing.
pub fn duration_field(
    temporary: bool,
    ttl: u64,
    disabled: bool,
    error_text: Option<&'static str>,
    on_change: OnDuration,
    cx: &App,
) -> Div {
    let c = cx.colors();
    let label = duration_label(ttl);
    // "Cleans up after 1 week without activity.", the duration emphasized.
    let description: AnyElement = if temporary {
        let text = format!("Cleans up after {label}\u{a0}without activity.");
        let start = "Cleans up ".len();
        let end = start + "after ".len() + label.len();
        StyledText::new(text)
            .with_highlights([(
                start..end,
                HighlightStyle {
                    font_weight: Some(FontWeight::MEDIUM),
                    color: Some(c.text_standard),
                    ..Default::default()
                },
            )])
            .into_any_element()
    } else {
        "Cleans up without activity.".into_any_element()
    };
    let select = |value: Option<u64>| {
        let on_change = on_change.clone();
        move |window: &mut Window, cx: &mut App| on_change(value, window, cx)
    };
    let step = |id: &'static str, icon: IconName, tooltip: &'static str, to: Option<u64>| {
        let on_change = on_change.clone();
        IconButton::new(id, icon)
            .size(IconButtonSize::Sm)
            .variant(buzz_ui::ButtonVariant::Subtle)
            .surface(Surface::Floating)
            .tooltip(tooltip)
            .disabled(disabled || to.is_none())
            .on_click(move |_, window, cx| {
                if let Some(to) = to {
                    on_change(Some(to), window, cx);
                }
            })
    };
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::S3)
                .child(radio_card(
                    "duration-ongoing",
                    "Ongoing",
                    "Keeps its history until you archive it.".into_any_element(),
                    !temporary,
                    disabled,
                    select(None),
                    cx,
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            radio_card(
                                "duration-temporary",
                                "Temporary",
                                description,
                                temporary,
                                disabled,
                                select(Some(ttl)),
                                cx,
                            )
                            .flex_1(),
                        )
                        .when(temporary, |this| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .gap(space::S2)
                                    .p(space::S1)
                                    .pl(space::S3)
                                    .child(step(
                                        "duration-shorter",
                                        IconName::Minus,
                                        "Decrease duration",
                                        shorter(ttl),
                                    ))
                                    .child(step(
                                        "duration-longer",
                                        IconName::Plus,
                                        "Increase duration",
                                        longer(ttl),
                                    )),
                            )
                        }),
                ),
        )
        .children(error_text.map(|text| error(text, cx)))
}

// ---------------------------------------------------------------------------
// Privacy confirmation

const PRIVACY_KEY: &str = "channel-privacy-confirmation:dismissed";
/// Opt-outs saved before both directions shared one preference.
const LEGACY_PRIVACY_KEYS: [&str; 2] = [
    "channel-privacy-confirmation:private:dismissed",
    "channel-privacy-confirmation:public:dismissed",
];

/// "Don’t show me this again" was chosen in this community. A view
/// preference only, never authorization.
pub fn skip_privacy_confirmation(community: &Community, cx: &App) -> bool {
    let store = &cx.global::<AppState>().store;
    let partition = community.partition();
    std::iter::once(PRIVACY_KEY)
        .chain(LEGACY_PRIVACY_KEYS)
        .any(|key| matches!(store.view_state(&partition, key), Ok(Some(json)) if json == "true"))
}

pub fn remember_privacy_confirmation(community: &Community, cx: &App) {
    let saved = cx.global::<AppState>().store.set_view_state(
        &community.partition(),
        PRIVACY_KEY,
        Some("true"),
    );
    if let Err(error) = saved {
        tracing::warn!(%error, "could not remember the privacy confirmation choice");
    }
}

/// The title of the privacy step.
pub fn privacy_title(private: bool) -> &'static str {
    if private {
        "Make channel private?"
    } else {
        "Make channel public?"
    }
}

/// The privacy step's body: what changes, and the opt-out.
pub fn privacy_confirmation(
    private: bool,
    checked: bool,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(space::S4)
        .child(
            div()
                .text_body_sm()
                .text_color(cx.colors().text_subtle)
                .child(if private {
                    "Only channel members will have access."
                } else {
                    "Everyone in this community will be able to view this channel’s full history."
                }),
        )
        .child(
            Checkbox::new("privacy-skip", checked)
                .label("Don’t show me this again")
                .surface(Surface::Floating)
                .on_change(on_change),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn counts_from_ninety_percent_without_an_error() {
        assert_eq!(counter(107, 120, false), None);
        assert_eq!(counter(108, 120, false).as_deref(), Some("108/120"));
        assert_eq!(counter(120, 120, true), None);
        assert_eq!(counter(899, 1000, false), None);
        assert_eq!(counter(900, 1000, false).as_deref(), Some("900/1,000"));
        assert_eq!(counter(1234, 1000, false).as_deref(), Some("1,234/1,000"));
    }

    #[test]
    fn bounds_growth_at_the_caret_by_code_points() {
        // Typing within the limit is untouched.
        assert_eq!(bounded_input("abc", 3, 2, 3), None);
        // A paste in the middle loses its own overflow, not the tail.
        let (text, caret) = bounded_input("ab1234cd", 6, 4, 6).unwrap();
        assert_eq!(text, "ab12cd");
        assert_eq!(caret, 4);
        // Emoji count once each and are never split.
        let (text, caret) = bounded_input("😀😀😀", "😀😀😀".len(), 2, 2).unwrap();
        assert_eq!(text, "😀😀");
        assert_eq!(caret, "😀😀".len());
        // Text already above the limit stays; only new growth goes.
        assert_eq!(bounded_input("abcde", 5, 5, 3), None);
        let (text, _) = bounded_input("abcdef", 6, 5, 3).unwrap();
        assert_eq!(text, "abcde");
    }

    #[test]
    fn steps_between_a_day_and_two_weeks() {
        assert_eq!(shorter(86_400), None);
        assert_eq!(longer(86_400), Some(604_800));
        assert_eq!(shorter(604_800), Some(86_400));
        assert_eq!(longer(1_209_600), None);
        // A custom relay interval steps to its neighbours.
        assert_eq!(shorter(3 * 86_400), Some(86_400));
        assert_eq!(longer(3 * 86_400), Some(604_800));
        assert_eq!(longer(60), Some(86_400));
    }

    #[test]
    fn labels_durations_in_whole_units() {
        assert_eq!(duration_label(86_400), "1 day");
        assert_eq!(duration_label(604_800), "7 days");
        assert_eq!(duration_label(1_209_600), "2 weeks");
        assert_eq!(duration_label(7_200), "2 hours");
        assert_eq!(duration_label(60), "1 minute");
        assert_eq!(duration_label(61), "61 seconds");
    }
}
