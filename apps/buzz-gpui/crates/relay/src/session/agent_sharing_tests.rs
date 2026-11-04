use super::*;
use crate::outbox::{MemoryJournal, Outbox, OutboxOptions, Publisher};
use crate::reader::{Priority, Reader, ReaderOptions, Source};
use crate::{RelayHttp, RelayOrigin, Verifier};
use buzz_identity::{Identity, Purpose};
use futures::future::BoxFuture;
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use serde_json::Value;
use std::sync::{Arc, Mutex};

const CHANNEL: &str = "00000000-0000-4000-8000-000000000001";

#[derive(Default)]
struct SourceState {
    events: Vec<Event>,
    fail: bool,
    fail_publish: bool,
    posts: usize,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<SourceState>>);

impl Source for Fake {
    fn query(
        &self,
        filters: Vec<crate::Filter>,
        _: Priority,
    ) -> BoxFuture<'static, Result<Vec<Value>, crate::HttpError>> {
        let state = self.0.lock().unwrap();
        let result = if state.fail {
            Err(crate::HttpError::Transport("offline".into()))
        } else {
            let found = state
                .events
                .iter()
                .filter(|event| {
                    filters.iter().any(|filter| {
                        assert_eq!(
                            filter.get("consistency").and_then(Value::as_str),
                            Some("strong")
                        );
                        let kinds = filter["kinds"].as_array().unwrap();
                        let authors = filter["authors"].as_array().unwrap();
                        kinds
                            .iter()
                            .any(|k| k.as_u64() == Some(event.kind.as_u16() as u64))
                            && authors
                                .iter()
                                .any(|a| a.as_str() == Some(event.pubkey.to_hex().as_str()))
                            && ["h", "d"].iter().all(|name| {
                                filter.get(&format!("#{name}")).is_none_or(|values| {
                                    values.as_array().unwrap().iter().any(|v| {
                                        event.tags.iter().any(|tag| {
                                            tag.as_slice().first().map(String::as_str)
                                                == Some(*name)
                                                && tag.as_slice().get(1).map(String::as_str)
                                                    == v.as_str()
                                        })
                                    })
                                })
                            })
                    })
                })
                .map(|event| serde_json::to_value(event).unwrap())
                .collect();
            Ok(found)
        };
        Box::pin(async move { result })
    }
}

impl Publisher for Fake {
    fn post_event(&self, event: &Event) -> BoxFuture<'static, Result<Vec<u8>, crate::HttpError>> {
        let id = event.id.to_hex();
        let mut state = self.0.lock().unwrap();
        state.posts += 1;
        let result = if state.fail_publish {
            Err(crate::HttpError::Transport("delivery uncertain".into()))
        } else {
            state.events.push(event.clone());
            Ok(serde_json::to_vec(&json!({"event_id":id,"accepted":true,"message":""})).unwrap())
        };
        Box::pin(async move { result })
    }
    fn find_event(&self, _: &Event) -> BoxFuture<'static, Result<bool, String>> {
        Box::pin(async { Ok(false) })
    }
    fn read_echo(&self, _: EventId, _: Priority) -> BoxFuture<'static, Vec<Verified>> {
        Box::pin(async { Vec::new() })
    }
}

struct Fixture {
    ctx: Context,
    source: Fake,
    agent: Keys,
    relay: Keys,
}

impl Fixture {
    fn new() -> Self {
        let identity = Arc::new(Identity::ephemeral());
        let source = Fake::default();
        let relay = Keys::generate();
        let agent = Keys::generate();
        let origin = RelayOrigin::parse("http://127.0.0.1:9").unwrap();
        let reader = Reader::spawn(
            &tokio::runtime::Handle::current(),
            Arc::new(source.clone()),
            Arc::new(Verifier::default()),
            ReaderOptions::default(),
        );
        let ctx = Context {
            reader,
            http: RelayHttp::new(origin, identity.clone()),
            identity: identity.clone(),
            viewer: identity.pubkey_hex(),
            relay: relay.public_key().to_hex(),
            epoch: 1,
            outbox: Outbox::new(
                identity,
                Arc::new(source.clone()),
                Arc::new(MemoryJournal::new()),
                OutboxOptions::default(),
            ),
        };
        let fixture = Self {
            ctx,
            source,
            agent,
            relay,
        };
        fixture.channel(false, true);
        fixture.attest(&fixture.ctx.identity);
        fixture.listing("voltage.remote-workspace/sandbox");
        fixture
    }

    fn push(&self, event: Event) {
        self.source.0.lock().unwrap().events.push(event);
    }

    fn remove_kind(&self, kind: u16) {
        self.source
            .0
            .lock()
            .unwrap()
            .events
            .retain(|e| e.kind.as_u16() != kind);
    }

    fn event(&self, keys: &Keys, kind: u16, tags: Vec<Vec<String>>, content: String) -> Event {
        EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
            .custom_created_at(Timestamp::from_secs(now()))
            .finalize(keys)
            .unwrap()
    }

    fn channel(&self, archived: bool, member: bool) {
        for kind in [
            kinds::CHANNEL_METADATA,
            kinds::CHANNEL_ADMINS,
            kinds::CHANNEL_MEMBERS,
        ] {
            self.remove_kind(kind);
        }
        let mut meta = vec![
            vec!["d".into(), CHANNEL.into()],
            vec!["t".into(), "stream".into()],
            vec!["name".into(), "engineering".into()],
            vec!["public".into()],
        ];
        if archived {
            meta.push(vec!["archived".into(), "true".into()]);
        }
        self.push(self.event(&self.relay, kinds::CHANNEL_METADATA, meta, String::new()));
        self.push(self.event(
            &self.relay,
            kinds::CHANNEL_ADMINS,
            vec![
                vec!["d".into(), CHANNEL.into()],
                vec!["p".into(), self.ctx.viewer.clone(), "owner".into()],
            ],
            String::new(),
        ));
        let mut roster = vec![
            vec!["d".into(), CHANNEL.into()],
            vec![
                "p".into(),
                self.ctx.viewer.clone(),
                String::new(),
                "owner".into(),
            ],
        ];
        if member {
            roster.push(vec![
                "p".into(),
                self.agent.public_key().to_hex(),
                String::new(),
                "bot".into(),
            ]);
        }
        self.push(self.event(&self.relay, kinds::CHANNEL_MEMBERS, roster, String::new()));
    }

    fn attest(&self, owner: &Identity) {
        self.remove_kind(kinds::PROFILE);
        let auth: Vec<String> = serde_json::from_str(
            &owner
                .attest_agent(&self.agent.public_key().to_hex())
                .unwrap(),
        )
        .unwrap();
        self.push(self.event(
            &self.agent,
            kinds::PROFILE,
            vec![auth],
            json!({"name":"Brokkr"}).to_string(),
        ));
    }

    fn listing(&self, runtime: &str) {
        self.remove_kind(kinds::MANAGED_AGENT);
        let tags = vec![
            vec!["d".into(), self.agent.public_key().to_hex()],
            vec!["runtime".into(), runtime.into()],
        ];
        let unsigned = self
            .ctx
            .identity
            .template(
                Purpose::Content,
                kinds::MANAGED_AGENT,
                now(),
                tags,
                "{}".into(),
            )
            .unwrap();
        self.push(self.ctx.identity.sign(Purpose::Content, unsigned).unwrap());
    }

    fn grant(&self, enabled: bool) {
        let draft = Grant::draft(&self.agent.public_key().to_hex(), CHANNEL, enabled).unwrap();
        let unsigned = self
            .ctx
            .identity
            .template(
                Purpose::Content,
                draft.kind,
                now(),
                draft.tags,
                draft.content,
            )
            .unwrap();
        self.push(self.ctx.identity.sign(Purpose::Content, unsigned).unwrap());
    }
}

#[tokio::test]
async fn settings_read_uses_signed_membership_ownership_listing_and_permission() {
    let f = Fixture::new();
    let off = load(&f.ctx, CHANNEL).await.unwrap();
    assert_eq!(off.agents.len(), 1);
    assert!(!off.agents[0].enabled);
    assert!(off.agents[0].enable_reason.is_none());
    f.grant(true);
    assert!(load(&f.ctx, CHANNEL).await.unwrap().agents[0].enabled);
    f.attest(&Identity::ephemeral());
    assert!(load(&f.ctx, CHANNEL).await.unwrap().agents.is_empty());
    assert!(
        preflight(&f.ctx, CHANNEL, &f.agent.public_key().to_hex(), false)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn archive_removed_agent_and_runtime_change_block_enable_but_preserve_revoke() {
    let f = Fixture::new();
    let agent = f.agent.public_key().to_hex();
    assert!(preflight(&f.ctx, CHANNEL, &agent, true).await.is_ok());
    f.grant(true);
    f.channel(true, true);
    let state = load(&f.ctx, CHANNEL).await.unwrap();
    assert!(state.agents[0].enabled && state.agents[0].enable_reason.is_some());
    assert!(preflight(&f.ctx, CHANNEL, &agent, true).await.is_err());
    assert!(preflight(&f.ctx, CHANNEL, &agent, false).await.is_ok());
    f.channel(false, false);
    assert!(preflight(&f.ctx, CHANNEL, &agent, true).await.is_err());
    assert!(preflight(&f.ctx, CHANNEL, &agent, false).await.is_ok());
    f.channel(false, true);
    f.listing("buzz.codex/codex");
    assert!(preflight(&f.ctx, CHANNEL, &agent, true).await.is_err());
    assert!(preflight(&f.ctx, CHANNEL, &agent, false).await.is_ok());
}

#[tokio::test]
async fn failed_reads_never_turn_existing_permission_into_an_empty_success() {
    let f = Fixture::new();
    f.grant(true);
    assert!(load(&f.ctx, CHANNEL).await.unwrap().agents[0].enabled);
    f.source.0.lock().unwrap().fail = true;
    assert!(load(&f.ctx, CHANNEL).await.is_err());
}

#[tokio::test]
async fn changing_runtime_does_not_hide_a_saved_sharing_change() {
    let f = Fixture::new();
    f.ctx.outbox.ready().await.unwrap();
    let agent = f.agent.public_key().to_hex();
    let id = f
        .ctx
        .outbox
        .send_after(
            Grant::draft(&agent, CHANNEL, true).unwrap(),
            SendOptions {
                recovery: Some(Recovery {
                    key: recovery_key(CHANNEL, &agent),
                    value: "true".into(),
                }),
                ..SendOptions::default()
            },
            0,
        )
        .unwrap();
    let mut changes = f.ctx.outbox.changes();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while f.ctx.outbox.get(&id).unwrap().delivery == Delivery::Sending {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    f.remove_kind(kinds::AGENT_CHANNEL_GRANT);
    f.listing("buzz.codex/codex");
    let state = load(&f.ctx, CHANNEL).await.unwrap();
    assert_eq!(state.agents.len(), 1);
    assert_eq!(state.agents[0].pending.as_ref().unwrap().id, id);
    assert!(state.agents[0].enable_reason.is_some());
}

async fn delivery(f: &Fixture, id: EventId) -> Delivery {
    let mut changes = f.ctx.outbox.changes();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let delivery = f.ctx.outbox.get(&id).unwrap().delivery;
            if delivery != Delivery::Sending {
                return delivery;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn explicit_revoke_cancels_an_uncertain_enable_after_authority_changes() {
    for case in ["archive", "removed", "local"] {
        let f = Fixture::new();
        let agent = f.agent.public_key().to_hex();
        if case != "removed" {
            f.grant(false);
        }
        f.source.0.lock().unwrap().fail_publish = true;
        let change = prepare(&f.ctx, CHANNEL, &agent, true)
            .await
            .unwrap()
            .unwrap();
        let enable = f
            .ctx
            .outbox
            .send_after(change.draft, change.options, change.after)
            .unwrap();
        assert_eq!(delivery(&f, enable).await, Delivery::Unknown);
        match case {
            "archive" => f.channel(true, true),
            "removed" => f.channel(false, false),
            _ => f.listing("buzz.codex/codex"),
        }
        assert!(preflight(&f.ctx, CHANNEL, &agent, true).await.is_err());
        assert!(prepare(&f.ctx, CHANNEL, &agent, true).await.is_err());
        assert_eq!(f.source.0.lock().unwrap().posts, 1);
        let change = prepare(&f.ctx, CHANNEL, &agent, false)
            .await
            .unwrap()
            .unwrap();
        let revoke = f
            .ctx
            .outbox
            .send_after(change.draft, change.options, change.after)
            .unwrap();
        assert_eq!(delivery(&f, revoke).await, Delivery::Unknown);
        assert!(
            f.ctx.outbox.get(&revoke).unwrap().created_at()
                > f.ctx.outbox.get(&enable).unwrap().created_at()
        );
        assert!(f.ctx.outbox.get(&enable).unwrap().recovery.is_some());
        let state = load(&f.ctx, CHANNEL).await.unwrap();
        let shown = state.agents[0].pending.as_ref().unwrap();
        assert_eq!(shown.id, revoke);
        assert!(!shown.enabled);
        assert!(prepare(&f.ctx, CHANNEL, &agent, false).await.is_err());
        f.source.0.lock().unwrap().fail_publish = false;
        f.ctx.outbox.retry(&revoke, None).unwrap();
        assert_eq!(delivery(&f, revoke).await, Delivery::Accepted);
        confirm(&f.ctx, CHANNEL, &agent, revoke).await.unwrap();
        assert!(!load(&f.ctx, CHANNEL).await.unwrap().agents[0].enabled);
        assert!(f.ctx.outbox.get(&enable).unwrap().acknowledged);
        assert!(
            !f.ctx
                .outbox
                .pending()
                .iter()
                .any(|op| pending_for(op, CHANNEL, &agent))
        );
        let posts = f.source.0.lock().unwrap().posts;
        f.ctx.outbox.retry(&enable, None).unwrap();
        assert_eq!(f.source.0.lock().unwrap().posts, posts);
    }
}
