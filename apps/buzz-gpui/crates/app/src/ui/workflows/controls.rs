//! Small pieces the workflow views share: labelled fields, select menus,
//! the form accordion, panel tabs and confirmation dialogs
//! (`ConfirmAction.tsx`).

use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::settings::form::{self, FieldStyle};
use buzz_ui::{
    ActiveTheme, AlertDialog, Button, Icon, IconName, Menu, MenuItem, MenuSize, Scrim, Select,
    Surface, Tab, TabVariant, Tabs, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// Confirmation dialogs paint above the editor dialog; menus above both.
const CONFIRM_LAYER: usize = 2;

pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// `Field`: label, control, then the description or, instead, the error.
pub fn field(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    error: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    let style = FieldStyle {
        body: true,
        error_replaces: true,
        ..FieldStyle::default()
    };
    form::field(label, description, error, control, style, cx)
}

/// A secondary paragraph (`text-body-sm text-secondary`).
pub(crate) use crate::ui::settings::form::note;

/// An inline error (`text-danger`).
pub fn danger(text: impl Into<SharedString>, cx: &App) -> Div {
    div().text_color(cx.colors().text_danger).child(text.into())
}

/// A field select: the trigger, and its menu while `open`.
#[allow(clippy::too_many_arguments)]
pub fn select<V: 'static>(
    id: impl Into<SharedString>,
    value: Option<SharedString>,
    placeholder: &'static str,
    options: Vec<(SharedString, bool)>,
    open: bool,
    disabled: bool,
    toggle: impl Fn(&mut V, &mut Context<V>) + 'static,
    pick: impl Fn(&mut V, usize, &mut Window, &mut Context<V>) + 'static,
    dismiss: Dismiss,
    window: &Window,
    cx: &mut Context<V>,
) -> Div {
    let id: SharedString = id.into();
    let pick = Rc::new(pick);
    let mut list = div()
        .id(SharedString::from(format!("{id}-options")))
        .flex()
        .flex_col()
        .max_h(rems(22.))
        .overflow_y_scroll();
    for (index, (label, checked)) in options.into_iter().enumerate() {
        let pick = pick.clone();
        list = list.child(
            MenuItem::new((id.clone(), index), label)
                .checked(checked)
                .on_click(cx.listener(move |this, _, window, cx| {
                    pick(this, index, window, cx);
                    cx.notify();
                })),
        );
    }
    div()
        .relative()
        .w_full()
        .child(
            Select::new(id.clone())
                .map(|this| match value {
                    Some(value) => this.value(value),
                    None => this.placeholder(placeholder),
                })
                .open(open)
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, _, cx| {
                    toggle(this, cx);
                    cx.notify();
                })),
        )
        .when(open, |this| {
            this.child(overlay::dropdown(
                window,
                Align::Start,
                div()
                    .min_w(rems(15.))
                    .child(Menu::new().size(MenuSize::Wide).child(list)),
                dismiss,
            ))
        })
}

/// A labelled field select (`Select variant="field"` with its label).
#[allow(clippy::too_many_arguments)]
pub fn select_field<V: 'static>(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    value: Option<SharedString>,
    options: Vec<(SharedString, bool)>,
    open: bool,
    disabled: bool,
    toggle: impl Fn(&mut V, &mut Context<V>) + 'static,
    pick: impl Fn(&mut V, usize, &mut Window, &mut Context<V>) + 'static,
    dismiss: Dismiss,
    window: &Window,
    cx: &mut Context<V>,
) -> Div {
    let control = select(
        id, value, "", options, open, disabled, toggle, pick, dismiss, window, cx,
    );
    field(label, None, None, control, cx)
}

/// An accordion row (`Accordion variant="form"`): title, optional
/// caption under it, and the caret.
pub fn accordion_trigger(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    caption: Option<SharedString>,
    open: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    let c = cx.colors();
    let hover = c.affordance_selected;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap(space::S3)
        .w_full()
        .p(space::S2)
        .rounded(radius::ROW)
        .cursor_pointer()
        .hover(move |this| this.bg(hover))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::S1)
                .min_w_0()
                .child(
                    div()
                        .text_body()
                        .text_color(c.text_standard)
                        .child(title.into()),
                )
                .children(caption.map(|caption| {
                    div()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child(caption)
                })),
        )
        .child(
            Icon::new(if open {
                IconName::CaretUp
            } else {
                IconName::CaretDown
            })
            .size(16.),
        )
        .on_click(move |_, window, cx| on_toggle(window, cx))
}

/// Panel tabs (`Tabs variant="panel"`) over string values.
pub fn panel_tabs(
    id: &'static str,
    items: &[(&'static str, &'static str)],
    value: &str,
    on_change: impl Fn(&'static str, &mut Window, &mut App) + 'static,
) -> Tabs {
    let on_change = Rc::new(on_change);
    let mut tabs = Tabs::new(TabVariant::Panel);
    for (key, label) in items.iter().copied() {
        let on_change = on_change.clone();
        tabs = tabs.tab(
            Tab::new(SharedString::from(format!("{id}-{key}")), label)
                .selected(value == key)
                .on_click(move |_, window, cx| on_change(key, window, cx)),
        );
    }
    tabs
}

/// A full-window layer with the scrim, above the editor dialog.
pub fn layer(window: &Window, content: AnyElement, priority: usize) -> AnyElement {
    let size = window.viewport_size();
    deferred(
        anchored().position(point(px(0.), px(0.))).child(
            div()
                .id("workflow-layer")
                .occlude()
                .w(size.width)
                .h(size.height)
                .child(
                    Scrim::new().child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(content),
                    ),
                ),
        ),
    )
    .with_priority(priority)
    .into_any_element()
}

/// What a confirmation asks and offers.
pub struct Confirm {
    pub title: SharedString,
    pub description: SharedString,
    pub action: SharedString,
    pub cancel: SharedString,
    pub destructive: bool,
    pub pending: bool,
    pub error: Option<SharedString>,
    pub layer: usize,
}

impl Confirm {
    pub fn new(
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        action: impl Into<SharedString>,
    ) -> Self {
        Self {
            title: title.into(),
            description: description.into(),
            action: action.into(),
            cancel: "Keep editing".into(),
            destructive: false,
            pending: false,
            error: None,
            layer: CONFIRM_LAYER,
        }
    }

    /// Paints above a dialog that is itself above the editor.
    pub fn layer(mut self, layer: usize) -> Self {
        self.layer = layer;
        self
    }

    pub fn cancel(mut self, cancel: impl Into<SharedString>) -> Self {
        self.cancel = cancel.into();
        self
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    pub fn pending(mut self, pending: bool) -> Self {
        self.pending = pending;
        self
    }

    pub fn error(mut self, error: Option<SharedString>) -> Self {
        self.error = error;
        self
    }

    /// The alert dialog in its own layer. Escape cancels.
    pub fn render(
        self,
        id: &'static str,
        focus: &FocusHandle,
        on_confirm: Handler,
        on_cancel: Handler,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        if !focus.contains_focused(window, cx) {
            window.focus(focus, cx);
        }
        let cancel = on_cancel.clone();
        let confirm = on_confirm.clone();
        let action = Button::new(SharedString::from(format!("{id}-confirm")), self.action)
            .surface(Surface::Floating)
            .disabled(self.pending)
            .map(|button| {
                if self.destructive {
                    button.destructive()
                } else {
                    button.prominent()
                }
            })
            .on_click(move |_, window, cx| confirm(window, cx));
        let escape = on_cancel.clone();
        let pending = self.pending;
        let dialog = div()
            .track_focus(focus)
            .key_context(buzz_ui::DIALOG_CONTEXT)
            .on_action(move |_: &buzz_ui::Cancel, window, cx| {
                if !pending {
                    escape(window, cx);
                }
            })
            .child(
                AlertDialog::new(self.title)
                    .description(self.description)
                    .children(self.error.map(|error| danger(error, cx)))
                    .action(
                        Button::new(SharedString::from(format!("{id}-cancel")), self.cancel)
                            .surface(Surface::Floating)
                            .disabled(self.pending)
                            .on_click(move |_, window, cx| cancel(window, cx)),
                    )
                    .action(action),
            );
        layer(window, dialog.into_any_element(), self.layer)
    }
}

/// A `<details>` summary: a caret and its label, toggling what follows.
pub fn disclosure(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    open: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(space::S1)
        .cursor_pointer()
        .text_color(cx.colors().text_standard)
        .child(
            Icon::new(if open {
                IconName::CaretDown
            } else {
                IconName::CaretRight
            })
            .size(12.),
        )
        .child(label.into())
        .on_click(on_click)
}
