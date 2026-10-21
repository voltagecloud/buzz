//! What the Claude and Codex provider dialogs share: a select whose rows
//! carry a description (buzz-app's decorated `Select` options).

use super::tabs::Picking;
use crate::ui::overlay::{self, Align};
use buzz_ui::{ActiveTheme, Menu, MenuItem, MenuSize, Select, Surface, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// One option of a [`described_choice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl Choice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    pub fn described(mut self, description: Option<String>) -> Self {
        self.description = description.filter(|text| !text.is_empty());
        self
    }
}

/// A select showing `value` among `choices`, its menu below it while
/// `open` is `picker`.
#[allow(clippy::too_many_arguments)]
pub fn described_choice<T: Picking>(
    open: Option<T::Picker>,
    picker: T::Picker,
    id: &'static str,
    value: &str,
    choices: Vec<Choice>,
    disabled: bool,
    window: &Window,
    cx: &mut Context<T>,
) -> Div {
    let open = open == Some(picker) && !disabled;
    let label = choices
        .iter()
        .find(|choice| choice.value == value)
        .or(choices.first())
        .map(|choice| choice.label.clone())
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
        let subtle = cx.colors().text_subtle;
        let wide = choices.iter().any(|choice| choice.description.is_some());
        let items = choices.into_iter().map(|choice| {
            let checked = choice.value == value;
            let option = choice.value;
            MenuItem::new(SharedString::from(format!("{id}-{option}")), choice.label)
                .when_some(choice.description, |this, description| {
                    this.trailing(div().text_caption().text_color(subtle).child(description))
                })
                .checked(checked)
                .on_click(cx.listener(move |this: &mut T, _, _, cx| {
                    *this.open() = None;
                    this.pick(picker, option.clone(), cx);
                    cx.notify();
                }))
        });
        let menu = Menu::new()
            .when(wide, |menu| menu.size(MenuSize::Wide))
            .children(items);
        let page = cx.entity().downgrade();
        select = select.child(overlay::dropdown(
            window,
            Align::Start,
            div().min_w(rems(16.)).child(menu),
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
