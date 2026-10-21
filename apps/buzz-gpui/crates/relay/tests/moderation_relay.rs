//! Community administration against the local test relay: the NIP-43
//! roster, member commands and invite links. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`. Only throwaway keys are
//! added, promoted and removed; alice, bob and carol keep their roles.

use buzz_identity::Identity;
use buzz_relay_client::moderation::{Change, Grant, Member, Role};
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    origin: RelayOrigin,
    identities: Value,
}

fn fixture() -> Option<Fixture> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    Some(Fixture {
        origin: RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap(),
        identities,
    })
}

impl Fixture {
    fn identity(&self, name: &str) -> Arc<Identity> {
        let nsec = self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap();
        Arc::new(Identity::from_nsec(nsec).unwrap())
    }

    fn pubkey(&self, name: &str) -> String {
        self.identities["identities"][name]["pk_hex"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A session once the relay has identified itself.
    async fn session(&self, identity: Arc<Identity>) -> Session {
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            identity,
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        tokio::time::timeout(
            Duration::from_secs(20),
            snapshot.wait_for(|s| s.relay.is_some()),
        )
        .await
        .expect("the relay identifies itself")
        .unwrap();
        session
    }
}

/// Waits out the shared relay's API quota: a quota refusal or a paused
/// lane proves the request was not acted on, so it is sent again.
async fn paced<T, F: std::future::Future<Output = Result<T, String>>>(
    request: impl Fn() -> F,
) -> Result<T, String> {
    for _ in 0..3 {
        match request().await {
            Err(error)
                if error.starts_with("rate-limited:")
                    || error.starts_with("Relay requests paused") =>
            {
                let seconds = error
                    .rsplit("retry in ")
                    .next()
                    .and_then(|rest| rest.trim_end_matches('s').parse::<u64>().ok())
                    .unwrap_or(60);
                tokio::time::sleep(Duration::from_secs(seconds.min(90) + 1)).await;
            }
            result => return result,
        }
    }
    request().await
}

/// The session's administration calls, paced by the relay quota.
trait Paced {
    async fn roster(&self) -> Result<Option<Vec<Member>>, String>;
    async fn change(&self, change: Change) -> Result<(), String>;
    async fn mint(&self, ttl: u64, uses: Option<u32>) -> Result<moderation::Invite, String>;
}

impl Paced for Session {
    async fn roster(&self) -> Result<Option<Vec<Member>>, String> {
        paced(|| self.community_roster()).await
    }

    async fn change(&self, change: Change) -> Result<(), String> {
        next_second().await;
        paced(|| self.change_community_member(change.clone())).await
    }

    async fn mint(&self, ttl: u64, uses: Option<u32>) -> Result<moderation::Invite, String> {
        paced(|| self.mint_invite(ttl, uses)).await
    }
}

/// Sleeps into the next wall-clock second.
///
/// The relay signs each roster snapshot at the current second. A change
/// that brings the roster back to a member set already published in that
/// second, such as a demotion right after a promotion, makes an event
/// with the same id as the earlier snapshot. The relay then keeps the
/// snapshot in between as current until its reconcile sweep, up to a
/// minute later. buzz-admin bumps `created_at` past the newest roster for
/// this reason; the relay's own command path does not. One change per
/// second keeps every snapshot new.
async fn next_second() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    tokio::time::sleep(Duration::from_nanos(
        1_000_000_000 - u64::from(now.subsec_nanos()),
    ))
    .await;
}

fn role(members: &[Member], pubkey: &str) -> Option<Role> {
    members
        .iter()
        .find(|member| member.pubkey == pubkey)
        .map(|member| member.role)
}

/// Re-reads the roster until `pubkey` has `expected`, as the card does after
/// an accepted change.
async fn until_role(session: &Session, pubkey: &str, expected: Option<Role>) -> Vec<Member> {
    for _ in 0..40 {
        let members = session.roster().await.unwrap().unwrap();
        if role(&members, pubkey) == expected {
            return members;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the roster never showed {pubkey} as {expected:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_the_relay_signed_roster_and_roles() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session(fixture.identity("alice")).await;
    let members = alice.roster().await.unwrap().expect("a roster");
    assert_eq!(role(&members, &fixture.pubkey("alice")), Some(Role::Owner));
    assert_eq!(role(&members, &fixture.pubkey("bob")), Some(Role::Member));
    assert_eq!(role(&members, &fixture.pubkey("carol")), Some(Role::Member));

    // A member reads the same list and sees that it manages nothing.
    let bob = fixture.session(fixture.identity("bob")).await;
    let seen = bob.roster().await.unwrap().unwrap();
    assert!(!moderation::manages(role(&seen, &fixture.pubkey("bob"))));
}

#[tokio::test(flavor = "multi_thread")]
async fn mints_bounded_invite_links() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session(fixture.identity("alice")).await;
    let invite = alice.mint(3 * 24 * 60 * 60, None).await.unwrap();
    assert!(invite.url.ends_with(&format!("/invite/{}", invite.code)));
    assert_eq!(invite.max_uses, None);
    let once = alice.mint(24 * 60 * 60, Some(1)).await.unwrap();
    assert_eq!(once.max_uses, Some(1));
    assert_ne!(once.code, invite.code);

    // A plain member cannot; the relay's refusal stays generic natively.
    let bob = fixture.session(fixture.identity("bob")).await;
    let refused = bob.mint(600, None).await.unwrap_err();
    assert_eq!(refused, "Community request failed (403)");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_adds_promotes_demotes_and_removes_a_throwaway_member() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session(fixture.identity("alice")).await;
    let throwaway = Identity::ephemeral().pubkey_hex();

    alice
        .change(Change::Add {
            pubkey: throwaway.clone(),
            role: Grant::Member,
        })
        .await
        .unwrap();
    until_role(&alice, &throwaway, Some(Role::Member)).await;

    alice
        .change(moderation::Action::Promote.change(&throwaway))
        .await
        .unwrap();
    until_role(&alice, &throwaway, Some(Role::Admin)).await;

    alice
        .change(moderation::Action::Demote.change(&throwaway))
        .await
        .unwrap();
    until_role(&alice, &throwaway, Some(Role::Member)).await;

    // A member's command is refused verbatim and changes nothing.
    let bob = fixture.session(fixture.identity("bob")).await;
    let refused = bob
        .change(moderation::Action::Remove.change(&throwaway))
        .await
        .unwrap_err();
    assert_eq!(
        refused,
        "invalid: actor not authorized: must be admin or owner"
    );

    // The owner cannot be changed.
    let owner = fixture.pubkey("alice");
    let refused = alice
        .change(moderation::Action::Remove.change(&owner))
        .await
        .unwrap_err();
    assert!(refused.starts_with("invalid: cannot"), "{refused}");

    alice
        .change(moderation::Action::Remove.change(&throwaway))
        .await
        .unwrap();
    let members = until_role(&alice, &throwaway, None).await;
    assert_eq!(role(&members, &owner), Some(Role::Owner));
    assert_eq!(role(&members, &fixture.pubkey("bob")), Some(Role::Member));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_only_removes_members() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session(fixture.identity("alice")).await;
    let admin = Arc::new(Identity::ephemeral());
    let member = Identity::ephemeral().pubkey_hex();
    for (pubkey, role) in [
        (admin.pubkey_hex(), Grant::Admin),
        (member.clone(), Grant::Member),
    ] {
        alice.change(Change::Add { pubkey, role }).await.unwrap();
    }
    until_role(&alice, &member, Some(Role::Member)).await;
    until_role(&alice, &admin.pubkey_hex(), Some(Role::Admin)).await;

    let session = fixture.session(admin.clone()).await;
    let members = session.roster().await.unwrap().unwrap();
    assert_eq!(role(&members, &admin.pubkey_hex()), Some(Role::Admin));
    // Admins mint invites but cannot grant admin.
    session.mint(600, Some(1)).await.unwrap();
    let refused = session
        .change(moderation::Action::Promote.change(&member))
        .await
        .unwrap_err();
    assert!(
        refused.starts_with("invalid: actor not authorized"),
        "{refused}"
    );
    session
        .change(moderation::Action::Remove.change(&member))
        .await
        .unwrap();
    until_role(&alice, &member, None).await;

    alice
        .change(moderation::Action::Remove.change(&admin.pubkey_hex()))
        .await
        .unwrap();
    until_role(&alice, &admin.pubkey_hex(), None).await;
}
