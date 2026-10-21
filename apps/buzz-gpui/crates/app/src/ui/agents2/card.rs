//! An agent as a card (buzz-app's `AgentCard.tsx`): its portrait large,
//! its name, and one line about it; and the New agent tile
//! (`NewAgentTile.tsx`). Pack avatars show their bundled poster; any other
//! picture shows through its community's media access.

use super::packs;
use buzz_ui::{ActiveTheme, Avatar, AvatarSize, TypeStyled, tokens::radius};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// The card grid's columns at this viewport width (`AgentCard.module.css`).
pub fn columns(window: &Window) -> usize {
    let width = f32::from(window.viewport_size().width);
    if width <= 360. {
        1
    } else if width <= 560. {
        2
    } else {
        3
    }
}

/// The cards' grid.
pub fn grid(window: &Window) -> Div {
    div()
        .grid()
        .grid_cols(columns(window) as u16)
        .gap(rems(0.5))
}

/// The community whose media a picture is read through, by its `wss://`
/// or `https://` origin.
fn community(relay: &str, cx: &App) -> Option<Entity<crate::community::Community>> {
    let origin = crate::ui::agents::inventory::origin(relay);
    cx.global::<crate::community::AppState>()
        .communities
        .iter()
        .find(|community| community.read(cx).origin.http() == origin)
        .cloned()
}

/// A remote picture through its community's media access.
pub fn picture_source(picture: &str, relay: &str, cx: &App) -> Option<ImageSource> {
    if picture.is_empty() {
        return None;
    }
    // One from a community not added here (an import's) loads as is.
    Some(match community(relay, cx) {
        Some(community) => crate::ui::media::avatar(picture, community.read(cx).session.media()),
        None => crate::ui::media::any_avatar(picture),
    })
}

/// An agent's picture filling `size` rems: a pack avatar's poster, or any
/// other through its community's media access, with the initial when there
/// is none (`AgentPortrait`).
pub fn portrait(name: &str, picture: Option<&str>, relay: &str, size: f32, cx: &App) -> AnyElement {
    if let Some(pack) = picture.and_then(packs::by_url) {
        return img(pack.asset())
            .size(rems(size))
            .object_fit(ObjectFit::Contain)
            .into_any_element();
    }
    let rem = 16.;
    Avatar::new(name.to_owned())
        .size(AvatarSize::Px(size * rem))
        .squircle()
        .when_some(
            picture.and_then(|picture| picture_source(picture, relay, cx)),
            |avatar, source| avatar.image(source),
        )
        .into_any_element()
}

type Handler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// One card. `selected` is `Some` when the card is an option of the
/// inventory listbox.
#[derive(IntoElement)]
pub struct AgentCard {
    id: ElementId,
    name: SharedString,
    picture: Option<String>,
    relay: String,
    detail: Option<AnyElement>,
    icon: Option<AnyElement>,
    actions: Vec<AnyElement>,
    selected: Option<bool>,
    unavailable: bool,
    on_click: Option<Handler>,
}

impl AgentCard {
    pub fn new(
        id: impl Into<ElementId>,
        name: impl Into<SharedString>,
        picture: Option<String>,
        relay: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            picture,
            relay: relay.into(),
            detail: None,
            icon: None,
            actions: Vec::new(),
            selected: None,
            unavailable: false,
            on_click: None,
        }
    }

    /// One line under the name, such as its model.
    pub fn detail(mut self, detail: impl IntoElement) -> Self {
        self.detail = Some(detail.into_any_element());
        self
    }

    /// A mark before the detail, such as the type's or the model's.
    pub fn icon(mut self, icon: impl IntoElement) -> Self {
        self.icon = Some(icon.into_any_element());
        self
    }

    /// An action below the detail.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }

    /// Its type is unavailable: the portrait dims.
    pub fn unavailable(mut self, unavailable: bool) -> Self {
        self.unavailable = unavailable;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for AgentCard {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = *cx.colors();
        let pack = self.picture.as_deref().and_then(packs::by_url).is_some();
        let size = if pack { 8. } else { 6. };
        let option = self.selected.is_some();
        let selected = self.selected == Some(true);
        div()
            .id(self.id)
            .relative()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(rems(0.75))
            .min_w_0()
            .min_h(rems(15.25))
            .pt(rems(2.))
            .px(rems(0.75))
            .pb(rems(1.))
            .border_1()
            .border_color(if selected {
                c.border_focus
            } else {
                gpui_kit::transparent_black()
            })
            .rounded(radius::CONTAINER)
            .bg(c.affordance_subtle)
            .text_center()
            .when(option, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(c.affordance_subtle_hover))
            })
            .child(
                div()
                    .flex_none()
                    .size(rems(size))
                    .when(self.unavailable, |this| this.opacity(0.5))
                    .child(portrait(
                        &self.name,
                        self.picture.as_deref(),
                        &self.relay,
                        size,
                        cx,
                    )),
            )
            .child(div().max_w_full().truncate().child(self.name.clone()))
            .when_some(self.detail, |this, detail| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rems(0.25))
                        .max_w_full()
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .when_some(self.icon, |this, icon| this.child(icon))
                        .child(div().min_w_0().truncate().child(detail)),
                )
            })
            .when(!self.actions.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_center()
                        .gap(rems(0.5))
                        .children(self.actions),
                )
            })
            .when_some(self.on_click, |this, on_click| {
                this.on_click(move |event, window, cx| on_click(event, window, cx))
            })
    }
}

/// The pack avatars scattered behind the New agent label, and where each
/// sits: left or right offset, top or bottom offset (fractions of the
/// tile), and its turn in degrees.
const ARTWORK: [(&str, Side, Side, f32); 8] = [
    ("gloopies-3", Side::Left(-0.08), Side::Top(0.12), -12.),
    ("fuzzies-1", Side::Right(-0.09), Side::Top(0.08), 12.),
    ("pollies-23", Side::Left(0.34), Side::Top(0.), 0.),
    ("gloopies-20", Side::Left(0.35), Side::Bottom(0.), -10.),
    ("pollies-25", Side::Left(-0.10), Side::Bottom(0.10), 0.),
    ("fuzzies-4", Side::Right(-0.08), Side::Bottom(0.), 0.),
    ("pollies-24", Side::Right(-0.20), Side::Top(0.43), 0.),
    ("fuzzies-18", Side::Left(-0.24), Side::Top(0.45), 0.),
];

#[derive(Clone, Copy)]
enum Side {
    Left(f32),
    Right(f32),
    Top(f32),
    Bottom(f32),
}

/// The New agent tile: a scatter of pack avatars behind one label.
pub fn new_agent_tile(
    disabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let c = *cx.colors();
    // The tile's width is a grid column; its height is fixed.
    let height = 15.25;
    let artwork = ARTWORK.iter().filter_map(|(id, x, y, _turn)| {
        let avatar = packs::by_id(id)?;
        let place = |this: Img, side: Side| match side {
            Side::Left(f) => this.left(relative(f)),
            Side::Right(f) => this.right(relative(f)),
            Side::Top(f) => this.top(rems(height * f)),
            Side::Bottom(f) => this.bottom(rems(height * f)),
        };
        // GPUI cannot rotate an image; each sits upright.
        Some(place(
            place(
                img(avatar.asset())
                    .absolute()
                    .size(rems(4.5))
                    .object_fit(ObjectFit::Contain),
                *x,
            ),
            *y,
        ))
    });
    div()
        .id("new-agent-tile")
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .min_h(rems(height))
        .overflow_hidden()
        .border_1()
        .border_color(c.border_standard)
        .rounded(radius::CONTAINER)
        .bg(c.surface_panel)
        .map(|this| {
            if disabled {
                this.opacity(0.5)
            } else {
                this.cursor_pointer()
                    .hover(|this| this.bg(c.affordance_subtle_hover))
                    .on_click(on_click)
            }
        })
        .child(div().absolute().inset_0().children(artwork))
        .child(
            div()
                .relative()
                .py(rems(0.5))
                .px(rems(1.))
                .rounded_full()
                .bg(c.text_standard)
                .text_color(c.text_inverse)
                .text_label_sm()
                .child("New agent"),
        )
        .into_any_element()
}
