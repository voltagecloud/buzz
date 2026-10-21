//! What the agent types' tabs share: buzz-app's form fields, the select
//! their Settings tabs choose with, the model picker and instructions
//! field (`ModelPicker`, `AgentInstructions`), and the saved settings they
//! edit.

use crate::agents2::{AgentView, Agents2, ModelChoice};
use crate::ui::overlay::{self, Align};
use buzz_ui::{
    ActiveTheme, Button, Colors, Dialog, Icon, IconName, Menu, MenuItem, MenuSize, Select, Surface,
    TextArea, TextField, TypeStyled,
    tokens::{font, space},
};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::Value;
use std::rc::Rc;

/// The choices a select offers: its value and label.
pub type Options = Vec<(String, String)>;

/// `options` given as string literals.
pub fn options(options: &[(&str, &str)]) -> Options {
    options
        .iter()
        .map(|(value, label)| (value.to_string(), label.to_string()))
        .collect()
}

/// `buzz-field`: label, control, then the description.
pub fn field(
    label: &'static str,
    control: impl IntoElement,
    description: Option<SharedString>,
    c: &Colors,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .min_w_0()
        .child(div().text_label_sm().child(label))
        .child(control)
        .children(description.map(|text| description_text(text, c)))
}

/// `buzz-field-description`.
pub fn description_text(text: impl Into<SharedString>, c: &Colors) -> Div {
    div()
        .text_body_sm()
        .text_color(c.text_subtle)
        .child(text.into())
}

/// A description with one word in code type: words wrap one at a time,
/// and punctuation after the code word stays with it.
pub fn note_with_code(before: &str, code: &str, after: &str, c: &Colors) -> Div {
    let word = |text: &str, code: bool| {
        div()
            .when(code, |this| this.font_family(font::MONO))
            .child(SharedString::from(text.to_string()))
    };
    let (attached, after) = match after.split_once(' ') {
        Some((first, rest)) if first.starts_with([',', '.']) => (first, rest),
        _ => ("", after),
    };
    div()
        .flex()
        .flex_wrap()
        .gap_x(rems(0.25))
        .text_body_sm()
        .text_color(c.text_subtle)
        .children(before.split(' ').map(|text| word(text, false)))
        .child(
            div()
                .flex()
                .child(word(code, true))
                .child(word(attached, false)),
        )
        .children(after.split(' ').map(|text| word(text, false)))
}

/// The agent's saved settings, as they are now: they can change in
/// another view. With a provider change staged, the destination's staged
/// settings.
pub fn saved_config(view: &AgentView, cx: &App) -> Value {
    if let Some(staged) = view.staged() {
        return staged.clone();
    }
    Agents2::global(cx)
        .and_then(|agents| {
            agents
                .read(cx)
                .find(&view.agent.pubkey)
                .map(|agent| agent.config.clone())
        })
        .unwrap_or_else(|| view.agent.config.clone())
}

/// A view with selects, at most one of them open.
pub trait Picking: Sized + 'static {
    type Picker: Copy + PartialEq + 'static;
    /// The open select, if any.
    fn open(&mut self) -> &mut Option<Self::Picker>;
    /// Takes `value` from `picker`'s menu; the menu is already closed.
    fn pick(&mut self, picker: Self::Picker, value: String, cx: &mut Context<Self>);
}

/// A select showing `value` among `options`, its menu below it while
/// `open` is `picker`.
#[allow(clippy::too_many_arguments)]
pub fn choice<T: Picking>(
    open: Option<T::Picker>,
    picker: T::Picker,
    id: &'static str,
    value: &str,
    options: Options,
    disabled: bool,
    window: &Window,
    cx: &mut Context<T>,
) -> Div {
    let open = open == Some(picker) && !disabled;
    let label = options
        .iter()
        .find(|(option, _)| option == value)
        .or(options.first())
        .map(|(_, label)| label.clone())
        .unwrap_or_default();
    let mut select = div().relative().w_full().child(
        Select::new(id)
            .value(label)
            .open(open)
            .surface(Surface::Floating)
            .disabled(disabled)
            .on_click(cx.listener(move |this: &mut T, _, _, cx| {
                let open = this.open();
                *open = (*open != Some(picker)).then_some(picker);
                cx.notify();
            })),
    );
    if open {
        let items = options.into_iter().map(|(option, label)| {
            MenuItem::new(SharedString::from(format!("{id}-{option}")), label)
                .checked(option == value)
                .on_click(cx.listener(move |this: &mut T, _, _, cx| {
                    *this.open() = None;
                    this.pick(picker, option.clone(), cx);
                    cx.notify();
                }))
        });
        let page = cx.entity().downgrade();
        select = select.child(overlay::dropdown(
            window,
            Align::Start,
            div().min_w(rems(16.)).child(Menu::new().children(items)),
            Rc::new(move |_, cx| {
                let _ = page.update(cx, |this, cx| {
                    *this.open() = None;
                    cx.notify();
                });
            }),
        ));
    }
    select
}

/// The instructions placeholder (`AgentInstructions`).
pub const INSTRUCTIONS_PLACEHOLDER: &str = "Describe what I should do…";

/// The instructions editor a Settings tab shows, with its placeholder.
pub fn instructions_state(value: &str, window: &mut Window, cx: &mut App) -> Entity<TextareaState> {
    let state = cx.new(|cx| TextareaState::new(window, cx).placeholder(INSTRUCTIONS_PLACEHOLDER));
    state.update(cx, |state, cx| {
        state.set_value(value.to_owned(), window, cx)
    });
    state
}

/// `AgentInstructions` in a settings tab: a plain "Instructions" field.
pub fn instructions_field(
    state: &Entity<TextareaState>,
    disabled: bool,
    description: &'static str,
    c: &Colors,
) -> Div {
    field(
        "Instructions",
        TextArea::new(state)
            .rows(8)
            .surface(Surface::Floating)
            .disabled(disabled),
        Some(description.into()),
        c,
    )
}

/// The picker value that opens the custom-model form rather than choosing.
pub const CUSTOM_MODEL: &str = "\0custom-model";

/// A model picker's rows: the type's default, its models, then a saved
/// model it does not list.
fn model_rows(
    models: &[ModelChoice],
    allow_custom: bool,
    value: &str,
    default_label: &str,
    default_description: Option<&str>,
) -> Vec<ModelChoice> {
    let mut rows = vec![ModelChoice {
        id: String::new(),
        label: default_label.to_owned(),
        description: default_description.map(str::to_owned),
    }];
    rows.extend(models.iter().cloned());
    if !value.is_empty() && !rows.iter().any(|row| row.id == value) {
        rows.push(ModelChoice {
            id: value.to_owned(),
            label: value.to_owned(),
            description: Some(if allow_custom {
                "Custom model".into()
            } else {
                "Unavailable model".into()
            }),
        });
    }
    rows
}

/// Whether `value` is a model the picker lists, or the default; the
/// custom-model form starts empty for those.
pub fn listed_model(models: &[ModelChoice], value: &str) -> bool {
    value.is_empty() || models.iter().any(|model| model.id == value)
}

/// `ModelPicker` as a field. `value` is a model id, or empty for the
/// type's default, `default_label`. Each row shows its model's mark; with
/// `allow_custom`, a last "Custom model" row picks [`CUSTOM_MODEL`].
#[allow(clippy::too_many_arguments)]
pub fn model_picker<T: Picking>(
    open: Option<T::Picker>,
    picker: T::Picker,
    id: &'static str,
    models: &[ModelChoice],
    allow_custom: bool,
    value: &str,
    default_label: &str,
    default_description: Option<&str>,
    loading: bool,
    disabled: bool,
    window: &Window,
    cx: &mut Context<T>,
) -> Div {
    let disabled = disabled || loading;
    let rows = model_rows(
        models,
        allow_custom,
        value,
        default_label,
        default_description,
    );
    let selected = rows
        .iter()
        .find(|row| row.id == value)
        .unwrap_or(&rows[0])
        .clone();
    let is_open = open == Some(picker) && !disabled;
    let icon = |row: &ModelChoice, cx: &App| {
        crate::ui::agents2::model_icon(&format!("{} {}", row.id, row.label), 18., cx)
    };
    let select = Select::new(id)
        .value(selected.label.clone())
        .leading(icon(&selected, cx))
        .surface(Surface::Floating)
        .disabled(disabled)
        .on_click(cx.listener(move |this: &mut T, _, _, cx| {
            let open = this.open();
            *open = (*open != Some(picker)).then_some(picker);
            cx.notify();
        }));
    let page = cx.entity().downgrade();
    let dismiss: overlay::Dismiss = Rc::new(move |_, cx| {
        let _ = page.update(cx, |this, cx| {
            *this.open() = None;
            cx.notify();
        });
    });
    let c = *cx.colors();
    let mut menu = Menu::new().size(MenuSize::Wide);
    for (index, row) in rows.iter().enumerate() {
        let model = row.id.clone();
        menu = menu.child(
            MenuItem::new(
                SharedString::from(format!("{id}-{index}")),
                row.label.clone(),
            )
            .leading(icon(row, cx))
            .when_some(row.description.clone(), |this, description| {
                this.trailing(
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child(description),
                )
            })
            .checked(row.id == selected.id)
            .on_click(cx.listener(move |this: &mut T, _, _, cx| {
                *this.open() = None;
                this.pick(picker, model.clone(), cx);
                cx.notify();
            })),
        );
    }
    if allow_custom {
        menu = menu.child(
            MenuItem::new(SharedString::from(format!("{id}-custom")), "Custom model")
                .leading(Icon::new(IconName::Robot).size(18.).color(c.text_standard))
                .trailing(
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child("Enter a model ID"),
                )
                .on_click(cx.listener(move |this: &mut T, _, _, cx| {
                    *this.open() = None;
                    this.pick(picker, CUSTOM_MODEL.into(), cx);
                    cx.notify();
                })),
        );
    }
    field(
        "Model",
        overlay::select_menu(
            select,
            is_open,
            Align::Start,
            Some(rems(16.)),
            || menu.into_any_element(),
            dismiss,
            window,
        ),
        loading.then(|| "Loading models…".into()),
        &c,
    )
}

/// The custom model's id field, holding `initial`: a model id that is not
/// listed, or nothing.
pub fn custom_model_input(initial: &str, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("Enter a model ID")
            .default_value(initial.to_owned())
    })
}

/// `CustomModelDialog`: "Use model" hands the trimmed id to `submit`;
/// Cancel or a press outside calls `close`.
pub fn custom_model_dialog<T: 'static>(
    input: &Entity<InputState>,
    window: &Window,
    cx: &mut Context<T>,
    submit: impl Fn(&mut T, String, &mut Context<T>) + 'static,
    close: impl Fn(&mut T) + 'static,
) -> AnyElement {
    let empty = input.read(cx).value().trim().is_empty();
    let close = Rc::new(close);
    let weak = cx.entity().downgrade();
    let dismiss: overlay::Dismiss = {
        let close = close.clone();
        Rc::new(move |_, cx| {
            let close = close.clone();
            let _ = weak.update(cx, move |this, cx| {
                close(this);
                cx.notify();
            });
        })
    };
    let state = input.clone();
    let (on_close, on_cancel) = (close.clone(), close);
    let dialog = Dialog::new("custom-model", "Custom model")
        .compact_header_gap()
        .on_close(cx.listener(move |this, _, _, cx| {
            on_close(this);
            cx.notify();
        }))
        .child(TextField::new(input).surface(Surface::Floating))
        .footer(
            Button::new("custom-model-cancel", "Cancel")
                .surface(Surface::Floating)
                .on_click(cx.listener(move |this, _, _, cx| {
                    on_cancel(this);
                    cx.notify();
                })),
        )
        .footer(
            Button::new("custom-model-use", "Use model")
                .prominent()
                .disabled(empty)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let model = state.read(cx).value().trim().to_owned();
                    if !model.is_empty() {
                        submit(this, model, cx);
                        cx.notify();
                    }
                })),
        );
    overlay::modal(window, dialog, Some(dismiss)).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn choice(id: &str, label: &str) -> ModelChoice {
        ModelChoice {
            id: id.into(),
            label: label.into(),
            description: None,
        }
    }

    #[test]
    fn a_model_picker_lists_the_default_its_models_and_a_saved_unlisted_one() {
        let models = [choice("opus", "Opus")];
        let ids = |rows: Vec<ModelChoice>| -> Vec<String> {
            rows.into_iter().map(|row| row.id).collect()
        };
        assert_eq!(
            ids(model_rows(&models, true, "opus", "Claude default", None)),
            ["", "opus"]
        );
        let custom = model_rows(&models, true, "claude-x", "Claude default", None);
        assert_eq!(custom[2].description.as_deref(), Some("Custom model"));
        let gone = model_rows(&models, false, "gone", "Codex default", None);
        assert_eq!(gone[2].description.as_deref(), Some("Unavailable model"));
        assert!(listed_model(&models, "") && listed_model(&models, "opus"));
        assert!(!listed_model(&models, "claude-x"));
    }
}
