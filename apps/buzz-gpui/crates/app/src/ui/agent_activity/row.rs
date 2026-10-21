//! The shared working-agent row (`AgentActivityRow.tsx`,
//! `ActivityRows.module.css`): opening the conversation and viewing the
//! agent's activity are separate actions.

use buzz_ui::tokens::radius;
use buzz_ui::{ActiveTheme, Avatar, AvatarSize, IconButton, IconButtonSize, IconName, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

pub type Action = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct AgentActivityRow {
    pub id: SharedString,
    pub name: SharedString,
    pub picture: Option<ImageSource>,
    pub status: SharedString,
    /// The duration or timestamp beside the name.
    pub meta: Option<SharedString>,
    pub scope: Option<SharedString>,
    /// What opening the row does, for its tooltip.
    pub open_label: SharedString,
    pub on_open: Option<Action>,
    pub on_open_activity: Option<Action>,
}

impl RenderOnce for AgentActivityRow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = *cx.colors();
        let group = SharedString::from(format!("activity-row-{}", self.id));
        let with_action = self.on_open_activity.is_some();
        let mut avatar = Avatar::new(self.name.clone())
            .size(AvatarSize::Px(30.))
            .squircle()
            .cutout_color(c.surface_popover);
        if let Some(picture) = self.picture {
            avatar = avatar.image(picture);
        }
        let caption = |text: SharedString| {
            div()
                .min_w_0()
                .truncate()
                .text_caption()
                .text_color(c.text_subtle)
                .when(with_action, |this| this.pr(rems(2.)))
                .child(text)
        };
        let heading = div()
            .flex()
            .min_w_0()
            .items_baseline()
            .justify_between()
            .gap(rems(0.75))
            .text_body_sm()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text_standard)
                    .child(self.name.clone()),
            )
            .when_some(self.meta, |this, meta| {
                // The timestamp gives way to the action on hover.
                this.child(
                    div()
                        .flex_none()
                        .text_caption()
                        .text_color(c.text_subtle)
                        .when(with_action, |this| {
                            this.group_hover(group.clone(), |s| s.opacity(0.))
                        })
                        .child(meta),
                )
            });
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(rems(0.125))
            .child(heading)
            .child(caption(self.status))
            .when_some(self.scope, |this, scope| this.child(caption(scope)));
        let open = self.on_open.clone();
        let surface = div()
            .id(SharedString::from(format!("{}-open", self.id)))
            .flex()
            .items_center()
            .gap(rems(0.75))
            .min_h(rems(2.))
            .w_full()
            .rounded(radius::FLOATING_INNER)
            .map(|this| match &open {
                Some(open) => {
                    let open = open.clone();
                    this.py(rems(0.25))
                        .pl(rems(0.5))
                        .pr(rems(1.))
                        .cursor_pointer()
                        .hover(|s| s.bg(c.affordance_floating_hover))
                        .tooltip(Tooltip::text(self.open_label.clone()))
                        .on_click(move |_, window, cx| open(window, cx))
                }
                None => this.p(rems(0.5)),
            })
            .child(avatar)
            .child(body);
        div()
            .id(self.id.clone())
            .group(group.clone())
            .relative()
            .w_full()
            .py(rems(0.125))
            .child(surface)
            .when_some(self.on_open_activity, |this, view| {
                this.child(
                    div()
                        .absolute()
                        .top(rems(0.375))
                        .right(rems(0.25))
                        .p(rems(0.125))
                        .rounded(radius::ROW)
                        .bg(c.affordance_subtle_hover)
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .child(
                            IconButton::new(
                                SharedString::from(format!("{}-activity", self.id)),
                                IconName::ListBullets,
                            )
                            .size(IconButtonSize::Sm)
                            .tooltip("View activity")
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                view(window, cx)
                            }),
                        ),
                )
            })
    }
}

use buzz_ui::Tooltip;
