//! Membership rows (`membership-rows.ts`, `MembershipRow.tsx`): quiet
//! activity lines, with nearby joins, leaves and removals grouped into one.

use super::identity::Names;
use super::media;
use super::time;
use buzz_relay_client::Media;
use buzz_relay_client::fold::{MembershipChange, MembershipKind, MessageRow};
use buzz_ui::{ActiveTheme, Avatar, AvatarSize, Tooltip, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

/// Adjacent membership rows more than this apart never group.
const GROUP_GAP: u64 = 3600;

/// Whether `older` can join a group whose newest change is `newest`.
fn compatible(older: &MembershipChange, newest: &MembershipChange) -> bool {
    match newest.kind {
        MembershipKind::Joined => older.kind == MembershipKind::Joined,
        MembershipKind::Removed => {
            older.kind == MembershipKind::Removed && older.actor == newest.actor
        }
        MembershipKind::Left => older.kind == MembershipKind::Left,
    }
}

/// One timeline entry: a message, or a run of membership rows shown as
/// its newest row, timed at its oldest.
#[derive(Clone, Debug)]
pub struct Grouped {
    pub row: Arc<MessageRow>,
    /// The run, oldest first, when more than one row.
    pub group: Option<Arc<[Arc<MessageRow>]>>,
    /// The group's first time, for day dividers.
    pub created_at: u64,
}

/// `membershipRows`: newest-anchored groups, so loaded groups keep their
/// key when history is prepended. Messages, local days and gaps over an
/// hour break a group. A self-join followed by that person's departure is
/// one lifecycle.
pub fn group(rows: &[Arc<MessageRow>]) -> Vec<Grouped> {
    let mut result = Vec::with_capacity(rows.len());
    let mut end = rows.len();
    while end > 0 {
        let last = end - 1;
        let newest = &rows[last];
        let mut start = last;
        if let Some(change) = &newest.membership {
            let previous = last
                .checked_sub(1)
                .and_then(|i| rows[i].membership.as_ref());
            let lifecycle = change.kind == MembershipKind::Left
                && previous.is_some_and(|p| {
                    p.kind == MembershipKind::Joined
                        && p.actor == p.target
                        && p.target == change.target
                });
            while start > 0 {
                let (candidate, next) = (&rows[start - 1], &rows[start]);
                let Some(older) = &candidate.membership else {
                    break;
                };
                let fits = if lifecycle {
                    older.kind == MembershipKind::Joined
                        && older.actor == older.target
                        && older.target == change.target
                } else {
                    compatible(older, change)
                };
                if !time::same_day(candidate.created_at, next.created_at)
                    || next.created_at.saturating_sub(candidate.created_at) > GROUP_GAP
                    || !fits
                {
                    break;
                }
                start -= 1;
            }
        }
        result.push(if start == last {
            Grouped {
                row: newest.clone(),
                group: None,
                created_at: newest.created_at,
            }
        } else {
            Grouped {
                row: newest.clone(),
                group: Some(rows[start..=last].iter().cloned().collect()),
                created_at: rows[start].created_at,
            }
        });
        end = start;
    }
    result.reverse();
    result
}

/// `Intl.ListFormat` (en, conjunction): "A", "A and B", "A, B, and C".
pub fn list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// The people a run is about, in order, and its sentence and tooltip.
pub struct Description {
    pub targets: Vec<String>,
    pub text: String,
    pub title: String,
}

/// `membershipDescription`.
pub fn describe(rows: &[Arc<MessageRow>], names: &Names, viewer: &str) -> Description {
    let changes: Vec<&MembershipChange> =
        rows.iter().filter_map(|r| r.membership.as_ref()).collect();
    let Some(first) = changes.first() else {
        return Description {
            targets: Vec::new(),
            text: String::new(),
            title: String::new(),
        };
    };
    let mut targets: Vec<String> = Vec::new();
    for change in &changes {
        let target = change.target.to_hex();
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    let name = |id: &str, subject: bool| -> String {
        if id == viewer {
            if subject { "You" } else { "you" }.to_string()
        } else {
            names.display(id)
        }
    };
    let first_target = first.target.to_hex();
    let first_actor = first.actor.to_hex();
    let lead = name(&first_target, true);
    let mut rest: Vec<String> = targets
        .iter()
        .skip(1)
        .take(2)
        .map(|id| name(id, false))
        .collect();
    if targets.len() > 3 {
        let count = targets.len() - 3;
        rest.push(format!(
            "{count} {}",
            if count == 1 { "other" } else { "others" }
        ));
    }
    let rest = list(&rest);
    let subjects = if rest.is_empty() {
        lead.clone()
    } else {
        format!("{lead}, along with {rest},")
    };
    let you = first_target == viewer;
    let text = if first.kind == MembershipKind::Joined
        && changes
            .last()
            .is_some_and(|c| c.kind == MembershipKind::Left)
    {
        format!("{lead} joined and left the channel")
    } else if first.kind == MembershipKind::Left {
        format!("{subjects} left the channel")
    } else if first.kind == MembershipKind::Removed {
        let along = if rest.is_empty() {
            String::new()
        } else {
            format!(", along with {rest}")
        };
        format!(
            "{lead} {} removed by {}{along}",
            if you { "were" } else { "was" },
            name(&first_actor, false)
        )
    } else {
        let all_self = changes.iter().all(|c| c.actor == c.target);
        let all_added = changes.iter().all(|c| c.actor != c.target);
        let same_adder = all_added && changes.iter().all(|c| c.actor == first.actor);
        let action = if all_self {
            "joined".to_string()
        } else if same_adder {
            format!(
                "{} by {}",
                if you { "were added" } else { "added" },
                name(&first_actor, false)
            )
        } else if all_added {
            format!("{} added", if you { "were" } else { "was" })
        } else {
            "arrived".to_string()
        };
        let along = if rest.is_empty() {
            String::new()
        } else {
            format!("{} along with {rest}", if same_adder { "," } else { "" })
        };
        format!("{lead} {action}{along}")
    };
    let title = list(&targets.iter().map(|id| name(id, false)).collect::<Vec<_>>());
    Description {
        targets,
        text,
        title,
    }
}

/// The row: up to three 28px avatars with a panel ring, a "+N" chip, and
/// the sentence, centred, with every name in its tooltip.
pub fn render(
    row: &Arc<MessageRow>,
    group: Option<&[Arc<MessageRow>]>,
    names: &Names,
    media: &Media,
    cx: &App,
) -> AnyElement {
    let c = *cx.colors();
    let rows = group.map_or_else(|| vec![row.clone()], <[_]>::to_vec);
    let description = describe(&rows, names, names.viewer());
    let profiles = &names_profiles(names);
    let mut avatars = div().flex().flex_none().pr(px(6.));
    for id in description.targets.iter().take(3) {
        let name = names.display(id);
        let mut avatar = Avatar::new(name).size(AvatarSize::Px(16.));
        if names.is_library_agent(id) {
            avatar = avatar.squircle();
        }
        if let Some(picture) = profiles.get(id).and_then(|p| p.picture.as_deref()) {
            avatar = avatar.image(media::avatar(picture, media));
        }
        avatars = avatars.child(
            div()
                .size(px(20.))
                .mr(px(-6.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_2()
                .border_color(c.surface_panel)
                .bg(c.surface_panel)
                .overflow_hidden()
                .child(avatar),
        );
    }
    if description.targets.len() > 3 {
        avatars = avatars.child(
            div()
                .size(px(20.))
                .mr(px(-6.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_2()
                .border_color(c.surface_panel)
                .bg(c.affordance_selected)
                .text_caption()
                .text_color(c.text_subtle)
                .child(format!("+{}", description.targets.len() - 3)),
        );
    }
    div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .pt(px(8.))
        .pb(px(16.))
        .text_caption()
        .text_color(c.text_subtle)
        .child(avatars)
        .child(
            div()
                .id(SharedString::from(format!(
                    "membership-{}",
                    row.id.to_hex()
                )))
                .min_w_0()
                .when(!description.title.is_empty(), |d| {
                    d.tooltip(Tooltip::text(description.title.clone()))
                })
                .child(description.text),
        )
        .into_any_element()
}

fn names_profiles(names: &Names) -> buzz_relay_client::Profiles {
    names.profiles().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::identity::{self, AgentLibrary};
    use buzz_relay_client::{Profile, Profiles};
    use core::prelude::v1::test;
    use nostr::prelude::PublicKey;
    use std::collections::HashMap;

    fn key() -> PublicKey {
        buzz_identity::Identity::ephemeral().public_key()
    }

    fn change(
        at: u64,
        kind: MembershipKind,
        actor: PublicKey,
        target: PublicKey,
    ) -> Arc<MessageRow> {
        let mut row = super::super::markdown::tests::row("");
        row.id = nostr::prelude::EventId::from_byte_array([at as u8; 32]);
        row.created_at = 1_700_000_000 + at;
        row.membership = Some(MembershipChange {
            kind,
            actor,
            target,
        });
        Arc::new(row)
    }

    fn names(people: &[(PublicKey, &str)], viewer: &PublicKey) -> Arc<Names> {
        let profiles: Profiles = Arc::new(
            people
                .iter()
                .map(|(key, name)| {
                    (
                        key.to_hex(),
                        Arc::new(Profile {
                            name: (*name).into(),
                            ..Default::default()
                        }),
                    )
                })
                .collect::<HashMap<_, _>>(),
        );
        identity::names(
            &profiles,
            &AgentLibrary::default(),
            &viewer.to_hex(),
            None,
            None,
        )
    }

    #[test]
    fn groups_compatible_neighbours_newest_first() {
        let (a, b, c, d) = (key(), key(), key(), key());
        let rows = vec![
            change(1, MembershipKind::Joined, a, a),
            change(2, MembershipKind::Joined, b, b),
            change(3, MembershipKind::Left, c, c),
            change(4, MembershipKind::Joined, d, d),
            change(4000, MembershipKind::Joined, a, b),
        ];
        let grouped = group(&rows);
        assert_eq!(grouped.len(), 4);
        assert_eq!(grouped[0].group.as_ref().map(|g| g.len()), Some(2));
        assert_eq!(grouped[0].row.id, rows[1].id);
        assert_eq!(grouped[0].created_at, rows[0].created_at);
        assert!(grouped[3].group.is_none());
    }

    #[test]
    fn a_join_then_leave_is_one_lifecycle() {
        let a = key();
        let rows = vec![
            change(1, MembershipKind::Joined, a, a),
            change(2, MembershipKind::Left, a, a),
        ];
        let grouped = group(&rows);
        assert_eq!(grouped.len(), 1);
        let viewer = key();
        let names = names(&[(a, "Ann")], &viewer);
        let text = describe(
            grouped[0].group.as_deref().unwrap(),
            &names,
            &viewer.to_hex(),
        )
        .text;
        assert_eq!(text, "Ann joined and left the channel");
    }

    #[test]
    fn wording_follows_the_reference() {
        let (ann, bob, cat, dan, eve, viewer) = (key(), key(), key(), key(), key(), key());
        let names = names(
            &[
                (ann, "Ann"),
                (bob, "Bob"),
                (cat, "Cat"),
                (dan, "Dan"),
                (eve, "Eve"),
            ],
            &viewer,
        );
        let v = viewer.to_hex();
        let text = |rows: Vec<Arc<MessageRow>>| describe(&rows, &names, &v).text;
        assert_eq!(
            text(vec![change(1, MembershipKind::Joined, ann, ann)]),
            "Ann joined"
        );
        assert_eq!(
            text(vec![change(1, MembershipKind::Joined, bob, ann)]),
            "Ann added by Bob"
        );
        assert_eq!(
            text(vec![change(1, MembershipKind::Joined, bob, viewer)]),
            "You were added by Bob"
        );
        assert_eq!(
            text(vec![
                change(1, MembershipKind::Joined, eve, ann),
                change(2, MembershipKind::Joined, eve, bob),
            ]),
            "Ann added by Eve, along with Bob"
        );
        assert_eq!(
            text(vec![
                change(1, MembershipKind::Joined, eve, ann),
                change(2, MembershipKind::Joined, dan, bob),
            ]),
            "Ann was added along with Bob"
        );
        assert_eq!(
            text(vec![
                change(1, MembershipKind::Joined, ann, ann),
                change(2, MembershipKind::Joined, dan, bob),
            ]),
            "Ann arrived along with Bob"
        );
        assert_eq!(
            text(vec![
                change(1, MembershipKind::Left, ann, ann),
                change(2, MembershipKind::Left, bob, bob),
                change(3, MembershipKind::Left, cat, cat),
                change(4, MembershipKind::Left, dan, dan),
                change(5, MembershipKind::Left, eve, eve),
            ]),
            "Ann, along with Bob, Cat, and 2 others, left the channel"
        );
        assert_eq!(
            text(vec![change(1, MembershipKind::Removed, bob, viewer)]),
            "You were removed by Bob"
        );
        assert_eq!(
            text(vec![
                change(1, MembershipKind::Removed, eve, ann),
                change(2, MembershipKind::Removed, eve, bob),
            ]),
            "Ann was removed by Eve, along with Bob"
        );
        assert_eq!(list(&["A".into(), "B".into(), "C".into()]), "A, B, and C");
    }
}
