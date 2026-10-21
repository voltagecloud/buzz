//! Avatar and presence (6.13).

use crate::{ActiveTheme, Icon, IconName, TypeStyled, tokens::TextRole};
use gpui_kit::{
    AnyElement, App, ElementId, Hsla, ImageSource, InteractiveElement, IntoElement, ParentElement,
    Rems, RenderOnce, Role, SharedString, StatefulInteractiveElement, Styled, StyledImage, Window,
    div, img, prelude::FluentBuilder, rems, svg,
};

const SQUIRCLE: &str = "icons/custom/avatar-squircle.svg";
const CIRCLE_CUTOUT: &str = "icons/custom/avatar-circle-cutout.svg";
const SQUIRCLE_CUTOUT: &str = "icons/custom/avatar-squircle-cutout.svg";

/// Avatar box sizes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AvatarSize {
    /// 24×24, caption initial.
    Small,
    /// 32×32, body initial.
    #[default]
    Default,
    /// 40×40, body initial.
    Large,
    /// Any size in CSS px (`size="fill"` in a sized parent), body initial.
    Px(f32),
}

impl AvatarSize {
    fn px(self) -> f32 {
        match self {
            Self::Small => 24.,
            Self::Default => 32.,
            Self::Large => 40.,
            Self::Px(px) => px,
        }
    }
}

/// Humans are circles, agents squircles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AvatarShape {
    #[default]
    Circle,
    Squircle,
}

/// Presence badge (`statusBadge`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Presence {
    Online,
    Away,
    Offline,
}

/// An identity image with the shared neutral initial fallback.
#[derive(IntoElement)]
pub struct Avatar {
    name: SharedString,
    image: Option<ImageSource>,
    size: AvatarSize,
    shape: AvatarShape,
    presence: Option<Presence>,
    fallback_icon: Option<(IconName, f32)>,
    profile_disc: bool,
    cutout_color: Option<Hsla>,
    thinking: Option<(ElementId, bool)>,
    alt: Option<SharedString>,
}

impl Avatar {
    /// `name` supplies the fallback initial.
    pub fn new(name: impl Into<SharedString>) -> Self {
        Self {
            name: name.into(),
            image: None,
            size: AvatarSize::default(),
            shape: AvatarShape::default(),
            presence: None,
            fallback_icon: None,
            profile_disc: false,
            cutout_color: None,
            thinking: None,
            alt: None,
        }
    }

    pub fn image(mut self, source: impl Into<ImageSource>) -> Self {
        self.image = Some(source.into());
        self
    }

    pub fn size(mut self, size: AvatarSize) -> Self {
        self.size = size;
        self
    }

    pub fn shape(mut self, shape: AvatarShape) -> Self {
        self.shape = shape;
        self
    }

    pub fn squircle(self) -> Self {
        self.shape(AvatarShape::Squircle)
    }

    pub fn presence(mut self, presence: Option<Presence>) -> Self {
        self.presence = presence;
        self
    }

    /// An icon instead of the initial (Lightning 20, Robot 16, User 19).
    pub fn fallback_icon(mut self, icon: IconName, px: f32) -> Self {
        self.fallback_icon = Some((icon, px));
        self
    }

    /// The top-bar profile button look: a white disc with `#525252` text.
    pub fn profile_disc(mut self) -> Self {
        self.profile_disc = true;
        self
    }

    /// Color behind the avatar, used to fake the presence cutout over
    /// images (GPUI cannot mask images). Defaults to the panel fill.
    pub fn cutout_color(mut self, color: Hsla) -> Self {
        self.cutout_color = Some(color);
        self
    }

    /// An agent's thinking badge (`AgentAvatar`): while `thinking`, the
    /// online corner badge morphs into a pill of pulsing dots under the
    /// portrait. `id` keeps the morph's progress across frames.
    pub fn thinking(mut self, id: impl Into<ElementId>, thinking: bool) -> Self {
        self.thinking = Some((id.into(), thinking));
        self
    }

    /// The accessible name: an image labelled `alt`, with ", thinking",
    /// ", available" or the presence appended (`AgentAvatar`).
    pub fn alt(mut self, alt: impl Into<SharedString>) -> Self {
        self.alt = Some(alt.into());
        self
    }

    /// The first code point of the trimmed name, uppercased, or `?`.
    pub fn initial(name: &str) -> String {
        name.trim()
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect())
            .unwrap_or_else(|| "?".into())
    }
}

fn rem(px: f32) -> Rems {
    rems(px / 16.)
}

impl RenderOnce for Avatar {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let label = self.alt.take().map(|alt| {
            let state = match (self.thinking.as_ref().is_some_and(|t| t.1), self.presence) {
                (true, _) => ", thinking",
                (false, Some(Presence::Online)) => ", available",
                (false, Some(Presence::Away)) => ", away",
                (false, Some(Presence::Offline)) => ", offline",
                (false, None) => "",
            };
            let id = match &self.thinking {
                Some((id, _)) => id.clone(),
                None => ElementId::Name(alt.clone()),
            };
            (id, SharedString::from(format!("{alt}{state}")))
        });
        // The thinking badge replaces an online dot and morphs from it.
        let badge = self.thinking.take().and_then(|(id, thinking)| {
            let online = self.presence == Some(Presence::Online);
            if !thinking && !online {
                return None;
            }
            // The badge stands in for the presence dot.
            self.presence = None;
            let cutout = self.cutout_color;
            Some(super::thinking::badge(
                id,
                thinking,
                self.size.px(),
                cutout,
                window,
                cx,
            ))
        });
        let c = cx.colors();
        let size = self.size.px();
        let (fill, ink) = if self.profile_disc {
            (crate::colors::parse("#ffffff"), c.text_avatar_fallback)
        } else {
            (c.affordance_subtle, c.text_subtle)
        };
        let role = if self.size == AvatarSize::Small {
            TextRole::Caption
        } else {
            TextRole::Body
        };
        let squircle = self.shape == AvatarShape::Squircle;
        let cutout = self.presence.is_some();

        let has_image = self.image.is_some();
        let fallback = Fallback {
            initial: Self::initial(&self.name).into(),
            icon: self.fallback_icon,
            role,
            fill,
            ink,
            shape: match (squircle, cutout) {
                (false, false) => None,
                (true, false) => Some(SQUIRCLE),
                (false, true) => Some(CIRCLE_CUTOUT),
                (true, true) => Some(SQUIRCLE_CUTOUT),
            },
        };
        let body: AnyElement = match self.image {
            None => fallback.element(),
            Some(source) => img(source)
                .size_full()
                .map(|this| {
                    if squircle {
                        // Closest rounded-rect to the 0.07 squircle.
                        this.rounded(rem(size * 0.3))
                    } else {
                        this.rounded_full()
                    }
                })
                .with_fallback(move || fallback.element())
                .into_any_element(),
        };

        let cutout_color = self.cutout_color.unwrap_or(c.surface_panel);
        let dot = self.presence.map(|presence| {
            let (outer, inner) = match presence {
                Presence::Online => (c.status_avatar_online_border, c.status_avatar_online),
                Presence::Away => (c.status_away, c.status_away),
                Presence::Offline => (c.status_offline, c.status_offline),
            };
            let d = size * 0.25;
            let dot_shape = |color: Hsla, inset: f32| {
                let inner_size = d - inset * 2.;
                if squircle {
                    svg()
                        .path(SQUIRCLE)
                        .absolute()
                        .left(rem(inset))
                        .top(rem(inset))
                        .size(rem(inner_size))
                        .text_color(color)
                        .into_any_element()
                } else {
                    div()
                        .absolute()
                        .left(rem(inset))
                        .top(rem(inset))
                        .size(rem(inner_size))
                        .rounded_full()
                        .bg(color)
                        .into_any_element()
                }
            };
            div()
                .absolute()
                .left(rem(size * 0.85 - d / 2.))
                .top(rem(size * 0.85 - d / 2.))
                .size(rem(d))
                .child(dot_shape(outer, 0.))
                .when(inner != outer, |this| this.child(dot_shape(inner, 1.)))
        });
        // Over images the notch is faked with a ring in the backdrop color.
        let ring = (cutout && has_image).then(|| {
            let r = size * 0.4;
            div()
                .absolute()
                .left(rem(size * 0.85 - r / 2.))
                .top(rem(size * 0.85 - r / 2.))
                .size(rem(r))
                .rounded_full()
                .bg(cutout_color)
        });

        let root = div()
            .relative()
            .flex_none()
            .size(rem(size))
            .child(body)
            .children(ring)
            .children(dot)
            .children(badge);
        match label {
            Some((id, label)) => root
                .id(id)
                .role(Role::Image)
                .aria_label(label)
                .into_any_element(),
            None => root.into_any_element(),
        }
    }
}

/// The no-image look: the shape (with its notch) drawn as a vector, the
/// initial or icon centered on top.
#[derive(Clone)]
struct Fallback {
    initial: SharedString,
    icon: Option<(IconName, f32)>,
    role: TextRole,
    fill: Hsla,
    ink: Hsla,
    shape: Option<&'static str>,
}

impl Fallback {
    fn element(&self) -> AnyElement {
        let content: AnyElement = match self.icon {
            Some((icon, px)) => Icon::new(icon).size(px).color(self.ink).into_any_element(),
            None => div()
                .text_role(self.role)
                .text_color(self.ink)
                .child(self.initial.clone())
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .map(|this| match self.shape {
                None => this.rounded_full().bg(self.fill),
                Some(path) => this.child(
                    svg()
                        .path(path)
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .text_color(self.fill),
                ),
            })
            .child(div().relative().child(content))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::Avatar;

    #[test]
    fn initial_is_first_code_point_uppercased() {
        assert_eq!(Avatar::initial("  alice"), "A");
        assert_eq!(Avatar::initial(""), "?");
        assert_eq!(Avatar::initial("ßeta"), "SS");
    }
}
