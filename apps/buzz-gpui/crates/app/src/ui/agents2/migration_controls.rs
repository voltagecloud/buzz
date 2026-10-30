//! Semantic controls own focus, activation, and the accessible label. The
//! existing design-system components paint their contents without handlers.

use buzz_ui::{ActiveTheme, ButtonSize, Surface};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

type Click = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type Change = Rc<dyn Fn(bool, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub(super) struct Button {
    id: ElementId,
    label: SharedString,
    paint: buzz_ui::Button,
    disabled: bool,
    loading: bool,
    click: Option<Click>,
    focus: Option<FocusHandle>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        let id = id.into();
        let label = label.into();
        Self {
            paint: buzz_ui::Button::new(id.clone(), label.clone()),
            id,
            label,
            disabled: false,
            loading: false,
            click: None,
            focus: None,
        }
    }
    pub fn size(mut self, size: ButtonSize) -> Self {
        self.paint = self.paint.size(size);
        self
    }
    pub fn prominent(mut self) -> Self {
        self.paint = self.paint.prominent();
        self
    }
    pub fn surface(mut self, surface: Surface) -> Self {
        self.paint = self.paint.surface(surface);
        self
    }
    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.paint = self.paint.tooltip(text);
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self.paint = self.paint.disabled(disabled);
        self
    }
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self.paint = self.paint.loading(loading);
        self
    }
    pub fn on_click(
        mut self,
        click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.click = Some(Rc::new(click));
        self
    }

    /// Keep focus when the review button becomes the apply button.
    pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.focus = Some(focus.clone());
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let ring = cx.colors().border_focus;
        gpui_kit::base::Button::new(self.id)
            .accessibility_label(self.label)
            .disabled(self.disabled || self.loading)
            .when_some(self.focus, |button, focus| button.track_focus(&focus))
            .child(self.paint)
            .focus_visible(move |style| style.border_2().border_color(ring))
            .when_some(self.click, |button, click| {
                button.on_click(move |event, window, cx| click(event, window, cx))
            })
    }
}

#[derive(IntoElement)]
pub(super) struct Checkbox {
    id: ElementId,
    checked: bool,
    label: SharedString,
    change: Option<Change>,
}

impl Checkbox {
    pub fn new(id: impl Into<ElementId>, checked: bool) -> Self {
        Self {
            id: id.into(),
            checked,
            label: "".into(),
            change: None,
        }
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }
    pub fn on_change(mut self, change: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.change = Some(Rc::new(change));
        self
    }
}

impl RenderOnce for Checkbox {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let ring = cx.colors().border_focus;
        gpui_kit::base::Checkbox::new(self.id.clone())
            .checked(self.checked)
            .accessibility_label(self.label.clone())
            .child(buzz_ui::Checkbox::new(self.id, self.checked).label(self.label))
            .focus_visible(move |style| style.border_2().border_color(ring))
            .when_some(self.change, |checkbox, change| {
                checkbox.on_change(move |state, _, window, cx| {
                    change(state == gpui_kit::base::CheckboxState::Checked, window, cx)
                })
            })
    }
}
