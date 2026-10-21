//! Input and Textarea (6.3), wrapping gpui-kit's editing engine.

use crate::{ActiveTheme, Surface, TypeStyled, tokens::radius};
use gpui_kit::{
    AnyElement, App, Entity, Focusable, Hsla, IntoElement, ParentElement, RenderOnce, Styled,
    Window,
    component::input::{Input, InputState, Textarea, TextareaState},
    div,
    prelude::FluentBuilder,
    rems,
};

/// Border and fill for a field in its current state.
pub(crate) fn field_colors(
    focused: bool,
    invalid: bool,
    disabled: bool,
    surface: Surface,
    cx: &App,
) -> (Hsla, Hsla, Hsla) {
    let c = cx.colors();
    let border = if invalid {
        c.border_danger
    } else if focused {
        c.interaction(surface).border
    } else {
        super::TRANSPARENT
    };
    if disabled {
        (c.affordance_disabled, border, c.text_unavailable)
    } else {
        (c.surface_inset, border, c.text_standard)
    }
}

/// Field size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum FieldSize {
    Sm,
    #[default]
    Md,
}

/// A single-line text field: inset fill, radius 12, 1px border that turns
/// to the control color on focus and red when invalid.
///
/// The text, caret and placeholder come from gpui-kit's `Input` (themed by
/// [`crate::init`]); this draws the Buzz frame around it.
#[derive(IntoElement)]
pub struct TextField {
    state: Entity<InputState>,
    size: FieldSize,
    invalid: bool,
    disabled: bool,
    surface: Surface,
    leading: Option<AnyElement>,
    trailing: Option<AnyElement>,
}

impl TextField {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self {
            state: state.clone(),
            size: FieldSize::Md,
            invalid: false,
            disabled: false,
            surface: Surface::default(),
            leading: None,
            trailing: None,
        }
    }

    /// 32px high, body-sm.
    pub fn small(mut self) -> Self {
        self.size = FieldSize::Sm;
        self
    }

    pub fn invalid(mut self, invalid: bool) -> Self {
        self.invalid = invalid;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    /// InputGroup leading slot (icon in `text-subtle`).
    pub fn leading(mut self, element: impl IntoElement) -> Self {
        self.leading = Some(element.into_any_element());
        self
    }

    /// InputGroup trailing action (32px slot; end padding drops to 3px).
    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }
}

impl RenderOnce for TextField {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focused = self
            .state
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let (bg, border, text) =
            field_colors(focused, self.invalid, self.disabled, self.surface, cx);
        let subtle = cx.colors().text_subtle;
        let sm = self.size == FieldSize::Sm;
        let has_trailing = self.trailing.is_some();
        div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .w_full()
            .min_h(rems(if sm { 2. } else { 2.5 }))
            .pl(rems(1.))
            .pr(if has_trailing { rems(0.1875) } else { rems(1.) })
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(text)
            .map(|this| {
                if sm {
                    this.text_body_sm()
                } else {
                    this.text_body()
                }
            })
            .when_some(self.leading, |this, leading| {
                this.child(div().flex_none().text_color(subtle).child(leading))
            })
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.state)
                        .appearance(false)
                        .disabled(self.disabled)
                        .px_0()
                        .py_0()
                        .h(rems(1.25)),
                ),
            )
            .when_some(self.trailing, |this, trailing| {
                this.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .w(rems(2.))
                        .child(trailing),
                )
            })
    }
}

/// A multi-line field: same frame as [`TextField`], padding 16, min
/// height 96.
#[derive(IntoElement)]
pub struct TextArea {
    state: Entity<TextareaState>,
    invalid: bool,
    disabled: bool,
    surface: Surface,
    mono: bool,
    rows: usize,
}

impl TextArea {
    pub fn new(state: &Entity<TextareaState>) -> Self {
        Self {
            state: state.clone(),
            invalid: false,
            disabled: false,
            surface: Surface::default(),
            mono: false,
            rows: 3,
        }
    }

    /// Visible rows (3 by default, which fills the 96px minimum).
    pub fn rows(mut self, rows: usize) -> Self {
        self.rows = rows.max(1);
        self
    }

    pub fn invalid(mut self, invalid: bool) -> Self {
        self.invalid = invalid;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    /// `variant="code"`: JetBrains Mono 12/16.
    pub fn code(mut self) -> Self {
        self.mono = true;
        self
    }
}

impl RenderOnce for TextArea {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focused = self
            .state
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let (bg, border, text) =
            field_colors(focused, self.invalid, self.disabled, self.surface, cx);
        // gpui-kit pads the editor 8px × 10px inside; the frame supplies the
        // rest of buzz-app's 16px.
        let line = if self.mono { 1. } else { 1.25 };
        let editor_height = rems((self.rows as f32 * line + 1.).max(4.875));
        div()
            .w_full()
            .min_h(rems(6.))
            .px(rems(0.375))
            .py(rems(0.5))
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(text)
            .map(|this| {
                if self.mono {
                    this.text_mono()
                } else {
                    this.text_body()
                }
            })
            .child(
                Textarea::new(&self.state)
                    .appearance(false)
                    .disabled(self.disabled)
                    .h(editor_height),
            )
    }
}
