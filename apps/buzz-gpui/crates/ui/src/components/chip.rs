//! Inline chips, badges and keyboard shortcuts (6.14, 6.12).

use crate::{ActiveTheme, Icon, IconName, TypeStyled, tokens::radius};
use gpui_kit::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled, Window, div, prelude::FluentBuilder, relative, rems,
};

/// A reference chip in composer and message text: `@person`, `#channel`,
/// or a message / link reference with a leading icon.
#[derive(IntoElement)]
pub struct InlineChip {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    unresolved: bool,
}

impl InlineChip {
    /// `label` includes its sigil (`@alice`, `#general`).
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            unresolved: false,
        }
    }

    /// Leading icon for message (`ChatCircle`) and link (`Link`) chips.
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Inset fill, metadata text, inert.
    pub fn unresolved(mut self, unresolved: bool) -> Self {
        self.unresolved = unresolved;
        self
    }
}

impl RenderOnce for InlineChip {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let font_size = window.text_style().font_size;
        let (bg, fg) = if self.unresolved {
            (c.surface_inset, c.text_metadata)
        } else {
            (c.affordance_subtle, c.text_standard)
        };
        let hover = c.affordance_subtle_hover;
        let icon_px = font_size.to_pixels(window.rem_size()).as_f32() * 1.1;
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .max_w(rems(16.))
            .px(rems(0.1875))
            .rounded(radius::CHIP)
            .line_height(relative(1.35))
            .bg(bg)
            .text_color(fg)
            .overflow_hidden()
            .when(!self.unresolved, |this| {
                this.cursor_pointer().hover(move |s| s.bg(hover))
            })
            .when_some(self.icon, |this, icon| {
                this.child(
                    div()
                        .mr(rems(0.25))
                        .child(Icon::new(icon).size(icon_px * 16. / window.rem_size().as_f32())),
                )
            })
            .child(div().truncate().child(self.label))
    }
}

/// Badge tones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BadgeTone {
    /// Subtle fill, subtle text.
    #[default]
    Neutral,
    /// Prominent fill, inverse text.
    Prominent,
    /// Purple identity fill.
    Accent,
    Success,
    Warning,
    Danger,
}

/// A small caption pill for counts and states. buzz-app has no shared
/// Badge; this composes the roles its feature badges use. With no label it
/// renders the 6px unread dot.
#[derive(IntoElement)]
pub struct Badge {
    label: Option<SharedString>,
    tone: BadgeTone,
}

impl Badge {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: Some(label.into()),
            tone: BadgeTone::default(),
        }
    }

    /// The 6×6 unread / thread-activity dot.
    pub fn dot() -> Self {
        Self {
            label: None,
            tone: BadgeTone::Prominent,
        }
    }

    pub fn tone(mut self, tone: BadgeTone) -> Self {
        self.tone = tone;
        self
    }
}

impl RenderOnce for Badge {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        let (bg, fg) = match self.tone {
            BadgeTone::Neutral => (c.affordance_subtle, c.text_subtle),
            BadgeTone::Prominent => (c.affordance_prominent, c.text_inverse),
            BadgeTone::Accent => (c.affordance_accent, c.text_accent),
            BadgeTone::Success => (c.affordance_success, c.text_success),
            BadgeTone::Warning => (c.affordance_warning, c.text_warning),
            BadgeTone::Danger => (c.affordance_danger, c.text_danger),
        };
        match self.label {
            None => div().flex_none().size(rems(0.375)).rounded_full().bg(bg),
            Some(label) => div()
                .flex()
                .flex_none()
                .items_center()
                .h(rems(1.25))
                .px(rems(0.5))
                .rounded_full()
                .bg(bg)
                .text_color(fg)
                .text_caption()
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(label),
        }
    }
}

/// A keyboard shortcut chip (`kbd`): body-sm, padding 4×8, pill, inset
/// fill, subtle text.
#[derive(IntoElement)]
pub struct Kbd {
    keys: SharedString,
}

impl Kbd {
    pub fn new(keys: impl Into<SharedString>) -> Self {
        Self { keys: keys.into() }
    }
}

impl RenderOnce for Kbd {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.colors();
        div()
            .flex_none()
            .px(rems(0.5))
            .py(rems(0.25))
            .rounded_full()
            .bg(c.surface_inset)
            .text_color(c.text_subtle)
            .text_body_sm()
            .child(self.keys)
    }
}
