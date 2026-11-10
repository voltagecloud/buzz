use std::sync::Arc;

use buzz_core::{channel::MemberRole, TenantContext};
use buzz_db::EventQuery;
use buzz_sdk::sandbox_attach::{self as attach, snapshot, Scope};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, Tag, Timestamp};
use uuid::Uuid;

use crate::handlers::group_state_postgres_tests::{community, create_channel, http_auth, state};
use crate::handlers::ingest::ingest_event;
use crate::state::AppState;

struct Fixture {
    state: Arc<AppState>,
    tenant: TenantContext,
    owner: Keys,
    agent: Keys,
    scope: Scope,
}
impl Fixture {
    async fn new() -> Self {
        let state = state().await;
        let tenant = community(&state, "attach-claims").await;
        let (owner, agent) = (Keys::generate(), Keys::generate());
        for key in [&owner, &agent] {
            state
                .db
                .ensure_user(tenant.community(), key.public_key().as_bytes())
                .await
                .unwrap();
        }
        state
            .db
            .set_agent_owner(
                tenant.community(),
                agent.public_key().as_bytes(),
                owner.public_key().as_bytes(),
            )
            .await
            .unwrap();
        let channel = create_channel(&state, &tenant, &owner, &[]).await;
        state
            .db
            .add_member(
                tenant.community(),
                channel,
                agent.public_key().as_bytes(),
                MemberRole::Member,
                Some(owner.public_key().as_bytes()),
            )
            .await
            .unwrap();
        let scope = Scope {
            channel,
            thread: EventId::all_zeros(),
            agent: agent.public_key(),
        };
        Self {
            state,
            tenant,
            owner,
            agent,
            scope,
        }
    }

    fn events(&self, mode: &str) -> (Event, Event, Event, Event) {
        let now = Timestamp::now().as_secs();
        let session = Uuid::new_v4();
        if mode == "terminal" {
            let request = attach::request(&self.owner, self.scope, session, now + 600).unwrap();
            let pending = attach::Pending::parse(&request, now).unwrap();
            let approval = attach::approve(&self.owner, &pending, now + 600).unwrap();
            let grant = attach::Grant::parse(&approval, self.scope, now).unwrap();
            (
                request,
                approval,
                attach::audit(&self.agent, &grant, "accepted", 0, 0).unwrap(),
                attach::audit(&self.agent, &grant, "accepted", 0, 0).unwrap(),
            )
        } else {
            let request = snapshot::request(&self.owner, self.scope, session, now + 600).unwrap();
            let pending = snapshot::Pending::parse(&request, now).unwrap();
            let approval = snapshot::approve(&self.owner, &pending, now + 600).unwrap();
            let grant = snapshot::Grant::parse(&approval, self.scope, now).unwrap();
            (
                request,
                approval,
                snapshot::audit(&self.agent, &grant, "accepted", 0, None).unwrap(),
                snapshot::audit(&self.agent, &grant, "accepted", 0, None).unwrap(),
            )
        }
    }

    async fn publish(&self, event: Event, signer: &Keys) -> super::super::ingest::IngestResult {
        ingest_event(&self.state, &self.tenant, event, http_auth(signer))
            .await
            .unwrap()
    }

    async fn head(&self, event: &Event) -> Vec<buzz_core::StoredEvent> {
        self.state
            .db
            .query_events(&EventQuery {
                kinds: Some(vec![30624]),
                authors: Some(vec![event.pubkey.to_bytes().to_vec()]),
                d_tag: Some(d(event)),
                limit: Some(1),
                ..EventQuery::for_community(self.tenant.community())
            })
            .await
            .unwrap()
    }
}

fn d(event: &Event) -> String {
    event
        .tags
        .iter()
        .find(|tag| tag.as_slice()[0] == "d")
        .unwrap()
        .as_slice()[1]
        .clone()
}

fn rebuild(event: &Event, keys: &Keys, tags: Vec<Tag>, time: u64) -> Event {
    EventBuilder::new(event.kind, event.content.clone())
        .tags(tags)
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(time))
        .sign_with_keys(keys)
        .unwrap()
}

/// This drives the production ingest transaction on two concurrent writers.
/// A replacement/deletion/metadata purge cannot reopen either capability; the
/// per-write first-claim marker is never emitted for an exact replay or loser.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn attachment_acceptance_is_atomic_durable_and_narrowly_immutable() {
    let f = Fixture::new().await;
    for mode in ["terminal", "snapshot"] {
        let (request, approval, first, second) = f.events(mode);
        f.publish(request.clone(), &f.owner).await;
        f.publish(approval.clone(), &f.owner).await;
        assert_ne!(
            first.id, second.id,
            "same-second attempts have separate claim IDs"
        );
        let (a, b) = tokio::join!(
            ingest_event(&f.state, &f.tenant, first.clone(), http_auth(&f.agent)),
            ingest_event(&f.state, &f.tenant, second.clone(), http_auth(&f.agent)),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            [&a, &b]
                .into_iter()
                .filter(|reply| reply.message == attach::CLAIM_OK_MESSAGE)
                .count(),
            1
        );
        let winner = if a.message == attach::CLAIM_OK_MESSAGE {
            first
        } else {
            second
        };
        assert_eq!(f.head(&winner).await[0].event.id, winner.id);
        let replay = f.publish(winner.clone(), &f.agent).await;
        assert!(replay.message.starts_with("duplicate"));
        assert_ne!(replay.message, attach::CLAIM_OK_MESSAGE);

        for (kind, signer, tags) in [
            (5, &f.agent, vec![Tag::event(winner.id)]),
            (
                5,
                &f.agent,
                vec![Tag::parse([
                    "a",
                    &format!("30624:{}:{}", f.agent.public_key(), d(&winner)),
                ])
                .unwrap()],
            ),
            (
                9005,
                &f.owner,
                vec![
                    Tag::parse(["h", &f.scope.channel.to_string()]).unwrap(),
                    Tag::event(winner.id),
                ],
            ),
        ] {
            let deletion = EventBuilder::new(Kind::Custom(kind), "")
                .tags(tags)
                .sign_with_keys(signer)
                .unwrap();
            f.publish(deletion, signer).await;
            assert_eq!(
                f.head(&winner).await[0].event.id,
                winner.id,
                "accepted burn survived kind{kind}"
            );
        }
        // Request and owner approval still revoke through ordinary NIP-09.
        for target in [request.id, approval.id] {
            let revoke = EventBuilder::new(Kind::Custom(5), "")
                .tag(Tag::event(target))
                .sign_with_keys(&f.owner)
                .unwrap();
            f.publish(revoke, &f.owner).await;
        }
        assert!(f.head(&request).await.is_empty());
        assert!(f.head(&approval).await.is_empty());

        // Trusted operator retention may remove the audit body. The separate
        // first-write watermark still denies a never-submitted, newer claim.
        sqlx::query("DELETE FROM events WHERE community_id=$1 AND id=$2")
            .bind(f.tenant.community().as_uuid())
            .bind(winner.id.as_bytes().as_slice())
            .execute(f.state.db.pool())
            .await
            .unwrap();
        assert!(f.head(&winner).await.is_empty());
        let later = rebuild(
            &winner,
            &f.agent,
            winner.tags.clone().to_vec(),
            winner.created_at.as_secs() + 1,
        );
        let blocked = f.publish(later, &f.agent).await;
        assert_ne!(blocked.message, attach::CLAIM_OK_MESSAGE);
        assert!(
            f.head(&winner).await.is_empty(),
            "metadata purge reopened consumed approval"
        );
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn malformed_or_unowned_acceptance_never_gets_a_claim_receipt() {
    let f = Fixture::new().await;
    for mode in ["terminal", "snapshot"] {
        let (_, _, audit, _) = f.events(mode);
        let mut expiry = audit.tags.clone().to_vec();
        expiry.push(Tag::expiration(
            Timestamp::now() + std::time::Duration::from_secs(10),
        ));
        let mut ambiguous = audit.tags.clone().to_vec();
        ambiguous.push(Tag::parse(["d", "unrelated"]).unwrap());
        let mut missing_nonce = audit.tags.clone().to_vec();
        missing_nonce.retain(|tag| tag.as_slice()[0] != "buzz:attach-claim");
        let mut wrong_thread = audit.tags.clone().to_vec();
        for tag in &mut wrong_thread {
            if tag.as_slice()[0] == "e" {
                *tag = Tag::parse(["e", &"22".repeat(32)]).unwrap();
            }
        }
        for event in [
            rebuild(&audit, &f.agent, wrong_thread, audit.created_at.as_secs()),
            rebuild(&audit, &f.agent, expiry, audit.created_at.as_secs()),
            rebuild(&audit, &f.agent, ambiguous, audit.created_at.as_secs()),
            rebuild(&audit, &f.agent, missing_nonce, audit.created_at.as_secs()),
            rebuild(
                &audit,
                &f.owner,
                audit.tags.clone().to_vec(),
                audit.created_at.as_secs(),
            ),
        ] {
            assert!(ingest_event(
                &f.state,
                &f.tenant,
                event.clone(),
                http_auth(if event.pubkey == f.owner.public_key() {
                    &f.owner
                } else {
                    &f.agent
                })
            )
            .await
            .is_err());
            assert!(f.head(&audit).await.is_empty());
        }
        // An ordinary posting member cannot claim to be an agent merely by
        // obtaining an owner-signed envelope that names its key.
        let outsider = Keys::generate();
        f.state
            .db
            .ensure_user(f.tenant.community(), outsider.public_key().as_bytes())
            .await
            .unwrap();
        f.state
            .db
            .add_member(
                f.tenant.community(),
                f.scope.channel,
                outsider.public_key().as_bytes(),
                MemberRole::Member,
                Some(f.owner.public_key().as_bytes()),
            )
            .await
            .unwrap();
        let fake = Fixture {
            state: f.state.clone(),
            tenant: f.tenant.clone(),
            owner: f.owner.clone(),
            scope: Scope {
                agent: outsider.public_key(),
                ..f.scope
            },
            agent: outsider,
        };
        let (_, _, audit, _) = fake.events(mode);
        assert!(ingest_event(
            &fake.state,
            &fake.tenant,
            audit.clone(),
            http_auth(&fake.agent)
        )
        .await
        .is_err());
        assert!(fake.head(&audit).await.is_empty());
    }
}
