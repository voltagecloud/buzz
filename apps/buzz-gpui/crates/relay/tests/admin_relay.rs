//! Channel lifecycle, members, DMs, search and invites against the local
//! test relay. Skipped unless `BUZZ_TEST_RELAY` points at `.test-relay`.
//! Creates channels, DMs and a throwaway community member on the shared
//! relay; names carry the process id so runs do not collide.

use buzz_identity::Identity;
use buzz_relay_client::community::{self, Claimed, CommunityInput};
use buzz_relay_client::lifecycle::{Action, DetailsDraft, MemberRole, NewChannel};
use buzz_relay_client::members::{Change, MemberChange};
use buzz_relay_client::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

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

    async fn session(&self, name: &str) -> Session {
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            self.identity(name),
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the roster", |s| {
            s.roster == RosterStatus::Ready && s.live
        })
        .await;
        session
    }
}

async fn until<T: Clone>(
    receiver: &mut watch::Receiver<T>,
    what: &str,
    predicate: impl Fn(&T) -> bool,
) -> T {
    tokio::time::timeout(
        Duration::from_secs(20),
        receiver.wait_for(|value| predicate(value)),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    .unwrap()
    .clone()
}

fn joined(session: &Session, id: &str) -> Option<ChannelSummary> {
    session
        .snapshot()
        .borrow()
        .channels
        .iter()
        .find(|c| c.id == id)
        .cloned()
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

#[tokio::test(flavor = "multi_thread")]
async fn creates_edits_and_administers_a_channel() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.session("alice").await;
    let bob = fixture.pubkey("bob");
    let name = unique("admin");
    let id = alice
        .create_channel(NewChannel {
            name: format!("#{name}"),
            description: "Created by a test".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    let created = joined(&alice, &id).expect("the new channel is joined");
    assert_eq!(created.name, name);
    assert!(!created.read_only);

    let state = alice.channel_state(&id).await.unwrap();
    assert_eq!(state.role(alice.viewer()), Some(MemberRole::Owner));
    let permissions = state.lifecycle(alice.viewer()).unwrap();
    assert!(permissions.can_archive && !permissions.can_leave);

    let details = alice.channel_details(&id).await.unwrap();
    assert!(details.can_edit);
    let renamed = DetailsDraft {
        name: format!("{name}-renamed"),
        description: "Edited".into(),
        ..details.draft.clone()
    };
    alice
        .save_channel_details(&id, details.version, renamed.clone())
        .await
        .unwrap();
    let mut snapshot = alice.snapshot();
    until(&mut snapshot, "the new name", |s| {
        s.channels
            .iter()
            .any(|c| c.id == id && c.name == renamed.name)
    })
    .await;
    let stale = alice
        .save_channel_details(&id, details.version, details.draft)
        .await;
    assert!(stale.unwrap_err().contains("changed"), "optimistic version");

    alice.add_member(&id, &bob).await.unwrap();
    let state = alice.channel_state(&id).await.unwrap();
    assert_eq!(state.role(&bob), Some(MemberRole::Member));
    // Mentioning a member passes the preflight; a stranger does not.
    alice
        .send(
            &id,
            "hello",
            messages::Mentions {
                recipients: vec![bob.clone()],
                references: Vec::new(),
                ..Default::default()
            },
            Vec::new(),
            None,
        )
        .await
        .unwrap();
    let stranger = fixture.pubkey("carol");
    let refused = alice
        .send(
            &id,
            "hello",
            messages::Mentions {
                recipients: vec![stranger],
                references: Vec::new(),
                ..Default::default()
            },
            Vec::new(),
            None,
        )
        .await;
    assert!(refused.is_err());

    alice
        .change_member(
            &id,
            MemberChange {
                pubkey: bob.clone(),
                expected: MemberRole::Member,
                change: Change::Admin,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        alice.channel_state(&id).await.unwrap().role(&bob),
        Some(MemberRole::Admin)
    );
    alice
        .change_member(
            &id,
            MemberChange {
                pubkey: bob.clone(),
                expected: MemberRole::Admin,
                change: Change::Remove,
            },
        )
        .await
        .unwrap();
    assert_eq!(alice.channel_state(&id).await.unwrap().role(&bob), None);

    alice.channel_action(Action::Archive, &id).await.unwrap();
    until(&mut snapshot, "the archived flag", |s| {
        s.channels.iter().any(|c| c.id == id && c.archived)
    })
    .await;
    alice.channel_action(Action::Unarchive, &id).await.unwrap();
    until(&mut snapshot, "the unarchived flag", |s| {
        s.channels.iter().any(|c| c.id == id && !c.archived)
    })
    .await;
    // Leave no throwaway channel behind.
    alice.channel_action(Action::Delete, &id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn previews_joins_and_leaves_a_public_channel() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session("alice").await;
    let name = unique("open");
    let id = alice
        .create_channel(NewChannel {
            name: name.clone(),
            description: String::new(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();

    let bob = fixture.session("bob").await;
    let found = bob.search_public_channels(&name, true).await.unwrap();
    let preview = found.iter().find(|c| c.id == id).expect("public preview");
    assert!(preview.read_only);
    assert!(joined(&bob, &id).is_none());

    bob.channel_action(Action::Join, &id).await.unwrap();
    let mut snapshot = bob.snapshot();
    until(&mut snapshot, "the joined channel", |s| {
        s.channels.iter().any(|c| c.id == id && !c.read_only)
    })
    .await;
    bob.channel_action(Action::Leave, &id).await.unwrap();
    until(&mut snapshot, "the channel to leave the list", |s| {
        s.channels.iter().all(|c| c.id != id)
    })
    .await;
    alice.channel_action(Action::Delete, &id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn opens_hides_and_reopens_a_dm() {
    let Some(fixture) = fixture() else {
        return;
    };
    let carol = fixture.session("carol").await;
    let (alice, bob) = (fixture.pubkey("alice"), fixture.pubkey("bob"));
    let id = carol
        .open_dm(vec![alice.clone(), bob.clone()])
        .await
        .unwrap();
    let dm = joined(&carol, &id).expect("the group DM is joined");
    assert_eq!(dm.channel_type, Some(ChannelType::Dm));
    assert_eq!(dm.participants.as_ref().map(Vec::len), Some(2));

    carol.channel_action(Action::Hide, &id).await.unwrap();
    let mut snapshot = carol.snapshot();
    until(&mut snapshot, "the hidden DM", |s| {
        s.hidden_dms.contains(&id)
    })
    .await;
    let again = carol.open_dm(vec![bob, alice]).await.unwrap();
    assert_eq!(again, id, "the same participants reuse the DM");
    until(&mut snapshot, "the DM to unhide", |s| {
        s.hidden_dms.iter().all(|h| *h != id)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn searches_people_and_messages() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session("alice").await;
    let bob = fixture.pubkey("bob");
    let people = alice.people("bo", 1).await.unwrap();
    assert!(people.people.iter().any(|p| p.pubkey == bob), "{people:?}");
    assert!(people.people.iter().all(|p| p.pubkey != alice.viewer()));
    let members = alice.search_members(&bob, 1).await.unwrap();
    assert_eq!(members.people.len(), 1);

    let token = unique("needle").replace('-', "");
    let random = joined_named(&alice, "random");
    alice
        .send(
            &random,
            &format!("search {token} here"),
            Default::default(),
            Vec::new(),
            None,
        )
        .await
        .unwrap();
    let mut outcome = SearchOutcome::default();
    for _ in 0..20 {
        outcome = alice
            .search_messages(MessageSearch {
                text: token.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        if !outcome.hits.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let hit = outcome.hits.first().expect("the new message is indexed");
    assert_eq!(hit.channel, random);
    assert!(hit.preview.contains(&token));
    assert!(outcome.channels.iter().any(|c| c.id == random));

    // Operators narrow it: by author name and by channel.
    let by_alice = alice
        .search_messages(MessageSearch {
            text: token.clone(),
            from: Some("@alice".into()),
            channel: Some(random.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(by_alice.hits.len(), 1, "{by_alice:?}");
    let by_bob = alice
        .search_messages(MessageSearch {
            text: token,
            from: Some(bob),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(by_bob.hits.is_empty());
}

fn joined_named(session: &Session, name: &str) -> String {
    session
        .snapshot()
        .borrow()
        .channels
        .iter()
        .find(|c| c.name == name)
        .unwrap()
        .id
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_an_invite_link() {
    let Some(fixture) = fixture() else {
        return;
    };
    // The owner mints a one-use invite.
    let owner = RelayHttp::new(fixture.origin.clone(), fixture.identity("alice"));
    let minted = owner
        .post(
            "/api/invites",
            json!({"ttl_secs": 600, "max_uses": 1}).to_string(),
        )
        .await
        .unwrap();
    let minted: Value = serde_json::from_slice(&minted).unwrap();
    let code = minted["code"].as_str().unwrap();
    let link = format!(
        "buzz://join?relay={}&code={code}",
        fixture.origin.websocket()
    );
    let input = community::parse_input(&link).unwrap();
    let CommunityInput::Invite { origin, code } = input else {
        panic!("an invite");
    };
    assert_eq!(origin, fixture.origin);

    let newcomer = RelayHttp::new(origin, Arc::new(Identity::ephemeral()));
    let info = community::inspect(&newcomer).await.unwrap();
    assert!(info.policy.is_none());
    assert!(!community::probe(&newcomer).await.unwrap());
    let claimed = community::claim(&newcomer, &code, None, false)
        .await
        .unwrap();
    assert_eq!(claimed, Claimed::Joined);
    assert!(community::probe(&newcomer).await.unwrap());
    let again = community::claim(&newcomer, &code, None, false).await;
    assert!(
        matches!(again, Ok(Claimed::AlreadyMember) | Err(_)),
        "{again:?}"
    );
}

/// A throwaway community member: claims a fresh invite from alice.
async fn newcomer(fixture: &Fixture) -> Arc<Identity> {
    let owner = RelayHttp::new(fixture.origin.clone(), fixture.identity("alice"));
    let minted = owner
        .post(
            "/api/invites",
            json!({"ttl_secs": 600, "max_uses": 1}).to_string(),
        )
        .await
        .unwrap();
    let minted: Value = serde_json::from_slice(&minted).unwrap();
    let identity = Arc::new(Identity::ephemeral());
    let http = RelayHttp::new(fixture.origin.clone(), identity.clone());
    let code = minted["code"].as_str().unwrap();
    community::claim(&http, code, None, false).await.unwrap();
    identity
}

/// A throwaway member that is `owner`'s agent: its kind 0 carries
/// `owner`'s NIP-OA attestation. `None` when the owner link could not be
/// recorded (see [`link_agent_owner`]).
async fn agent_of(fixture: &Fixture, owner: &str) -> Option<Session> {
    let agent = newcomer(fixture).await;
    let attestation = fixture
        .identity(owner)
        .attest_agent(&agent.pubkey_hex())
        .unwrap();
    let tag: Vec<String> = serde_json::from_str(&attestation).unwrap();
    let purpose = buzz_identity::Purpose::Content;
    let now = nostr::prelude::Timestamp::now().as_secs();
    let profile = json!({"name": unique("agent")}).to_string();
    let unsigned = agent.template(purpose, 0, now, vec![tag], profile).unwrap();
    let event = agent.sign(purpose, unsigned).unwrap();
    RelayHttp::new(fixture.origin.clone(), agent.clone())
        .post("/events", serde_json::to_string(&event).unwrap())
        .await
        .unwrap();
    if !link_agent_owner(&agent.pubkey_hex(), &fixture.pubkey(owner)) {
        return None;
    }
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        fixture.origin.clone(),
        agent,
        Store::in_memory().unwrap(),
    );
    let mut snapshot = session.snapshot();
    until(&mut snapshot, "the agent's roster", |s| {
        s.roster == RosterStatus::Ready && s.live
    })
    .await;
    Some(session)
}

/// The relay authorizes an owner from its own agent → owner link, which
/// it records when an agent authenticates through its owner (NIP-OA
/// admission, off on the plain test relay). The test writes that link for
/// its throwaway agent straight into the test relay's database.
fn link_agent_owner(agent: &str, owner: &str) -> bool {
    let sql = format!(
        "UPDATE users SET agent_owner_pubkey = decode('{owner}', 'hex') \
         WHERE pubkey = decode('{agent}', 'hex')"
    );
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "buzz-gpui-test-postgres",
            "psql",
            "-U",
            "buzz",
            "-d",
            "buzz",
        ])
        .args(["-tAc", &sql])
        .output();
    match output {
        Ok(output) if output.status.success() && output.stdout.starts_with(b"UPDATE 1") => true,
        other => {
            eprintln!("could not link the agent's owner on the test relay: {other:?}");
            false
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn owners_delete_a_channel() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session("alice").await;
    let id = alice
        .create_channel(NewChannel {
            name: unique("delete"),
            description: String::new(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    let bob = fixture.session("bob").await;
    bob.channel_action(Action::Join, &id).await.unwrap();
    // A member may not delete it; the owner may.
    let member = bob.channel_permissions(&id).await.unwrap();
    assert!(!member.can_delete && !member.delete_unavailable);
    assert!(bob.channel_action(Action::Delete, &id).await.is_err());
    assert!(alice.channel_permissions(&id).await.unwrap().can_delete);
    alice.channel_action(Action::Delete, &id).await.unwrap();
    assert!(joined(&alice, &id).is_none());
    assert!(alice.channel_state(&id).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_owner_removes_it_and_deletes_its_channel() {
    let Some(fixture) = fixture() else {
        return;
    };
    let Some(agent) = agent_of(&fixture, "bob").await else {
        eprintln!("the owner link needs the test relay's database; skipping");
        return;
    };
    let agent_key = agent.viewer().to_owned();
    let bob = fixture.session("bob").await;
    let carol = fixture.session("carol").await;
    bob.want_profiles(vec![agent_key.clone()]);
    let mut snapshot = bob.snapshot();
    let profiles = until(&mut snapshot, "the agent's profile", |s| {
        s.profiles.contains_key(&agent_key)
    })
    .await;
    assert_eq!(
        profiles.profiles[&agent_key].attested_owner.as_deref(),
        Some(bob.viewer())
    );

    // In alice's channel, bob (a plain member) may remove his own agent
    // but carol may not.
    let alice = fixture.session("alice").await;
    let shared = alice
        .create_channel(NewChannel {
            name: unique("owned-agent"),
            description: String::new(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    for session in [&agent, &bob, &carol] {
        session.channel_action(Action::Join, &shared).await.unwrap();
    }
    let state = bob.channel_state(&shared).await.unwrap();
    let remove = MemberChange {
        pubkey: agent_key.clone(),
        expected: state.role(&agent_key).unwrap(),
        change: Change::Remove,
    };
    assert!(carol.change_member(&shared, remove.clone()).await.is_err());
    bob.change_member(&shared, remove).await.unwrap();
    let state = bob.channel_state(&shared).await.unwrap();
    assert_eq!(state.role(&agent_key), None);

    // The agent's own channel: its owner may delete it, others may not.
    let owned = agent
        .create_channel(NewChannel {
            name: unique("agent-made"),
            description: String::new(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    bob.channel_action(Action::Join, &owned).await.unwrap();
    carol.channel_action(Action::Join, &owned).await.unwrap();
    let permissions = bob.channel_permissions(&owned).await.unwrap();
    assert!(permissions.can_delete, "{permissions:?}");
    assert!(!carol.channel_permissions(&owned).await.unwrap().can_delete);
    bob.channel_action(Action::Delete, &owned).await.unwrap();
    assert!(joined(&bob, &owned).is_none());
    // Leave no throwaway channel behind.
    alice.channel_action(Action::Delete, &shared).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn publishes_a_profile_after_claiming_an_invite() {
    let Some(fixture) = fixture() else {
        return;
    };
    let joiner = newcomer(&fixture).await;
    let http = RelayHttp::new(fixture.origin.clone(), joiner.clone());
    let journal = community::JoinJournal::new(Store::in_memory().unwrap(), joiner.pubkey_hex());
    journal.begin(fixture.origin.websocket(), None).unwrap();
    let found = community::inspect_profile(&http).await.unwrap().unwrap();
    assert_eq!(found.profile, None, "a new member has no profile yet");
    let profile = ProfileEdit {
        name: unique("Newcomer"),
        picture: String::new(),
        about: "Joined by a test".into(),
    };
    let step = journal
        .begin(fixture.origin.websocket(), Some(profile.clone()))
        .unwrap();
    community::publish_profile(&http, &profile, &found)
        .await
        .unwrap();
    let confirmed = community::inspect_profile(&http).await.unwrap().unwrap();
    assert!(confirmed.matches(&profile), "{confirmed:?}");
    journal.finish(&step).unwrap();
    assert_eq!(journal.latest().unwrap(), None);

    // Publishing again replaces it.
    let renamed = ProfileEdit {
        name: unique("Renamed"),
        ..profile
    };
    community::publish_profile(&http, &renamed, &confirmed)
        .await
        .unwrap();
    let again = community::inspect_profile(&http).await.unwrap().unwrap();
    assert!(again.matches(&renamed));
}
