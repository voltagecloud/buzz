//! Scoped appearance: buzz-app's `.light` / `.dark` subtree classes.

use crate::theme::{Appearance, Theme, ThemeStack};
use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};

type Build = Box<dyn FnOnce(&mut Window, &mut App) -> AnyElement>;

/// Lays out and paints its content with a forced appearance, so
/// `cx.theme()` inside returns that palette. Used for `colorMode="dark"`
/// popups and for the gallery's side-by-side columns.
///
/// The content is built lazily, inside the scope, because Buzz components
/// read the theme when they render.
pub struct ThemeScope {
    appearance: Appearance,
    build: Option<Build>,
}

impl ThemeScope {
    pub fn new<E: IntoElement>(
        appearance: Appearance,
        build: impl FnOnce(&mut Window, &mut App) -> E + 'static,
    ) -> Self {
        Self {
            appearance,
            build: Some(Box::new(move |window, cx| {
                build(window, cx).into_any_element()
            })),
        }
    }

    fn with<R>(&self, cx: &mut App, f: impl FnOnce(&mut App) -> R) -> R {
        let scoped = Theme::global(cx).scoped(self.appearance);
        cx.global_mut::<ThemeStack>().0.push(scoped);
        let result = f(cx);
        cx.global_mut::<ThemeStack>().0.pop();
        result
    }
}

impl IntoElement for ThemeScope {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for ThemeScope {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let build = self.build.take().expect("ThemeScope laid out twice");
        self.with(cx, |cx| {
            let mut element = build(window, cx);
            let layout_id = element.request_layout(window, cx);
            (layout_id, element)
        })
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.with(cx, |cx| element.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.with(cx, |cx| element.paint(window, cx));
    }
}
