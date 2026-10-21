//! Community admin (`buzz.moderation`): Settings → Administration →
//! Membership for owners and admins, and the rail menu's "Invite to
//! community".

mod card;
mod invite;
mod store;

use crate::community::AppState;
use crate::ui::settings::cards::{self, CardPlacement, CardVisibility, SettingsCard};
use buzz_relay_client::SessionStatus;
use gpui_kit::*;

/// The card's key; the rail menu opens Settings here (`MEMBERSHIP_SECTION`).
pub const MEMBERSHIP: &str = "buzz.moderation/membership";

pub fn init(cx: &mut App) {
    store::init(cx);
    cards::register(
        SettingsCard {
            key: MEMBERSHIP,
            title: "Membership",
            icon: None,
            placement: CardPlacement::Administration,
            visibility: Some(std::rc::Rc::new(visibility)),
            build: crate::ui::settings::cards::build(|window, cx| {
                cx.new(|cx| card::CommunityAdmin::new(window, cx)).into()
            }),
        },
        cx,
    );
    cards::register_check(MEMBERSHIP, store::ensure, cx);
}

/// Listed for managers; pending while the roster loads; an error when the
/// connection or the read failed (`index.tsx`).
fn visibility(cx: &App) -> CardVisibility {
    let failed = AppState::current(cx).is_none_or(|community| {
        matches!(community.read(cx).snapshot.status, SessionStatus::Failed(_))
    });
    visibility_of(&store::snapshot(cx), failed)
}

fn visibility_of(state: &store::Snapshot, failed: bool) -> CardVisibility {
    if state.can_manage() {
        CardVisibility::Visible
    } else if failed || state.error.is_some() {
        CardVisibility::Error
    } else if matches!(state.status, store::Status::Idle | store::Status::Loading)
        || state.refreshing
    {
        CardVisibility::Pending
    } else {
        CardVisibility::Hidden
    }
}

/// Whether the rail may offer "Invite to community" for the selected
/// community: its roster names the viewer an owner or admin.
pub fn manager(cx: &App) -> bool {
    store::snapshot(cx).can_manage()
}

/// Re-reads the selected community's roster for the rail menu that just
/// opened; registration is not authorization.
pub fn refresh_role(cx: &mut App) {
    store::reload(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::moderation::{Member, Role};
    use core::prelude::v1::test;
    use std::sync::Arc;

    fn state(status: store::Status, role: Option<Role>) -> store::Snapshot {
        store::Snapshot {
            status,
            members: Some(Arc::from(vec![Member {
                pubkey: "a".repeat(64),
                role: role.unwrap_or(Role::Member),
            }])),
            role,
            refreshing: false,
            error: None,
        }
    }

    #[test]
    fn the_card_shows_for_managers_and_waits_or_fails_otherwise() {
        use store::Status::*;
        assert_eq!(
            visibility_of(&state(Ready, Some(Role::Owner)), false),
            CardVisibility::Visible
        );
        assert_eq!(
            visibility_of(&state(Ready, Some(Role::Admin)), false),
            CardVisibility::Visible
        );
        // A manager stays listed even while the connection drops.
        assert_eq!(
            visibility_of(&state(Ready, Some(Role::Admin)), true),
            CardVisibility::Visible
        );
        assert_eq!(
            visibility_of(&state(Ready, Some(Role::Member)), false),
            CardVisibility::Hidden
        );
        assert_eq!(
            visibility_of(&state(Idle, None), false),
            CardVisibility::Pending
        );
        assert_eq!(
            visibility_of(&state(Loading, None), false),
            CardVisibility::Pending
        );
        assert_eq!(
            visibility_of(&state(Idle, None), true),
            CardVisibility::Error
        );
        let mut refreshing = state(Ready, Some(Role::Member));
        refreshing.refreshing = true;
        assert_eq!(visibility_of(&refreshing, false), CardVisibility::Pending);
        let mut failed = state(Error, None);
        failed.error = Some("Could not load members: offline".into());
        assert_eq!(visibility_of(&failed, false), CardVisibility::Error);
    }

    #[test]
    fn the_rail_opens_the_registered_card_key() {
        // `MEMBERSHIP_SECTION` in buzz-app: plugin id, then the card id.
        assert_eq!(MEMBERSHIP, "buzz.moderation/membership");
    }
}
