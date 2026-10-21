//! "N unread" pills at the sidebar's edges (`SidebarUnread.tsx`): when
//! unread conversations are scrolled out of view above or below, a pill
//! says how many, with the faces of up to three unread direct messages,
//! and reveals the nearest one without opening it. A row in a collapsed
//! section is represented by the section's header.

use buzz_relay_client::Media;
use buzz_ui::{ActiveTheme, Avatar, AvatarSize, Icon, IconName, TypeStyled, tokens::shadow};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// A direct message's face in a pill.
#[derive(Clone, Debug, PartialEq)]
pub struct Face {
    pub name: SharedString,
    pub picture: Option<String>,
    pub agent: bool,
}

/// An unread conversation where it was last painted: its row, or its
/// collapsed section's header.
#[derive(Clone, Debug)]
pub struct Seen {
    pub channel: String,
    pub bounds: Bounds<Pixels>,
    pub face: Option<Face>,
}

/// The unread conversations out of view, nearest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Edges {
    pub above: Vec<(String, Option<Face>)>,
    pub below: Vec<(String, Option<Face>)>,
}

/// Splits conversations by where they sit against the viewport, counting
/// each once; any copy in view counts it as seen.
pub fn split(rows: &[Seen], viewport: Bounds<Pixels>) -> Edges {
    let mut edges = Edges::default();
    if viewport.size.height <= px(0.) {
        return edges;
    }
    let visible: Vec<&str> = rows
        .iter()
        .filter(|row| row.bounds.bottom() > viewport.top() && row.bounds.top() < viewport.bottom())
        .map(|row| row.channel.as_str())
        .collect();
    let mut above: Vec<&Seen> = rows
        .iter()
        .filter(|row| row.bounds.bottom() <= viewport.top())
        .collect();
    let mut below: Vec<&Seen> = rows
        .iter()
        .filter(|row| row.bounds.top() >= viewport.bottom())
        .collect();
    above.sort_by(|a, b| f32::from(b.bounds.top()).total_cmp(&f32::from(a.bounds.top())));
    below.sort_by(|a, b| f32::from(a.bounds.top()).total_cmp(&f32::from(b.bounds.top())));
    let nearest = |rows: Vec<&Seen>| {
        let mut seen: Vec<&str> = visible.clone();
        rows.into_iter()
            .filter(|row| {
                let first = !seen.contains(&row.channel.as_str());
                seen.push(&row.channel);
                first
            })
            .map(|row| (row.channel.clone(), row.face.clone()))
            .collect::<Vec<_>>()
    };
    edges.above = nearest(above);
    edges.below = nearest(below);
    edges
}

/// Reveals a conversation's row.
pub type OnReveal = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// The two pills, positioned over the list's frame.
pub fn pills(edges: &Edges, media: &Media, on_reveal: OnReveal, cx: &App) -> Vec<AnyElement> {
    [true, false]
        .into_iter()
        .filter_map(|above| pill(edges, above, media, on_reveal.clone(), cx))
        .collect()
}

fn pill(
    edges: &Edges,
    above: bool,
    media: &Media,
    on_reveal: OnReveal,
    cx: &App,
) -> Option<AnyElement> {
    let targets = if above { &edges.above } else { &edges.below };
    let (nearest, _) = targets.first()?;
    let c = cx.colors();
    let count = targets.len();
    let faces: Vec<&Face> = targets
        .iter()
        .filter_map(|t| t.1.as_ref())
        .take(3)
        .collect();
    let nearest = nearest.clone();
    Some(
        div()
            .absolute()
            .left_0()
            .right_0()
            .when(above, |d| d.top(px(20.)))
            .when(!above, |d| d.bottom_0())
            .flex()
            .justify_center()
            .child(
                div()
                    .id(if above {
                        "unread-above"
                    } else {
                        "unread-below"
                    })
                    .occlude()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .min_h(px(32.))
                    .px(px(16.))
                    .py(px(6.))
                    .rounded(px(12.))
                    .bg(c.affordance_prominent)
                    .shadow(shadow::sm())
                    .text_label_sm()
                    .text_color(c.text_inverse)
                    .cursor_pointer()
                    .tooltip(buzz_ui::Tooltip::text(format!(
                        "Reveal the nearest unread channel {} without opening it",
                        if above { "above" } else { "below" }
                    )))
                    .child(
                        Icon::new(if above {
                            IconName::ArrowUp
                        } else {
                            IconName::ArrowDown
                        })
                        .size(15.)
                        .color(c.text_inverse),
                    )
                    .when(!faces.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(div().flex().items_center().children(
                                    faces.iter().enumerate().map(|(index, face)| {
                                        let mut avatar = Avatar::new(face.name.clone())
                                            .size(AvatarSize::Px(20.));
                                        if face.agent {
                                            avatar = avatar.squircle();
                                        }
                                        if let Some(picture) = &face.picture {
                                            avatar =
                                                avatar.image(super::media::avatar(picture, media));
                                        }
                                        div()
                                            .when(index > 0, |d| d.ml(px(-6.)))
                                            .rounded_full()
                                            .border_2()
                                            .border_color(c.affordance_prominent)
                                            .child(avatar)
                                    }),
                                ))
                                .child("·"),
                        )
                    })
                    .child(div().truncate().child(format!("{count} unread")))
                    .on_click(move |_, window, cx| on_reveal(&nearest, window, cx)),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn row(channel: &str, top: f32) -> Seen {
        Seen {
            channel: channel.into(),
            bounds: Bounds::new(point(px(0.), px(top)), size(px(200.), px(28.))),
            face: None,
        }
    }

    #[test]
    fn counts_conversations_out_of_view_once_nearest_first() {
        let viewport = Bounds::new(point(px(0.), px(100.)), size(px(200.), px(300.)));
        let rows = vec![
            row("a", 0.),
            row("b", 40.),
            row("c", 200.),
            row("d", 500.),
            row("e", 600.),
            // A copy in view counts the conversation as seen.
            row("c", 700.),
            row("d", 650.),
        ];
        let edges = split(&rows, viewport);
        let names = |list: &Vec<(String, Option<Face>)>| {
            list.iter().map(|t| t.0.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(&edges.above), vec!["b", "a"]);
        assert_eq!(names(&edges.below), vec!["d", "e"]);
    }
}
