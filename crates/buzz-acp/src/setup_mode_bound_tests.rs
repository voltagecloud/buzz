use super::*;

#[tokio::test]
async fn bound_setup_nudges_stay_in_the_canonical_owner_thread() {
    for (case, expected) in [
        ("top-level", true),
        ("reply", true),
        ("foreign-thread", false),
        ("foreign-channel", false),
        ("dm", false),
        ("threaded-edit", true),
        ("top-level-edit", true),
        ("foreign-thread-edit", false),
        ("unresolved-edit", false),
    ] {
        let agent_keys = nostr::Keys::generate();
        let agent = agent_keys.public_key().to_hex();
        let owner_keys = nostr::Keys::generate();
        let owner = owner_keys.public_key().to_hex();
        let channel_id = Uuid::new_v4();
        let root = "11".repeat(32);
        let foreign_root = "22".repeat(32);
        let mut tags = vec![
            nostr::Tag::parse(["p", &agent]).unwrap(),
            nostr::Tag::parse(["h", &channel_id.to_string()]).unwrap(),
        ];
        if !matches!(case, "top-level" | "top-level-edit" | "unresolved-edit") {
            let thread = if case.starts_with("foreign-thread") {
                &foreign_root
            } else {
                &root
            };
            tags.push(nostr::Tag::parse(["e", thread, "", "reply"]).unwrap());
        }
        let original = nostr::EventBuilder::new(
            nostr::Kind::Custom(KIND_STREAM_MESSAGE as u16),
            "owner asks for setup",
        )
        .tags(tags)
        .sign_with_keys(&owner_keys)
        .unwrap();
        let event = if case.ends_with("edit") {
            nostr::EventBuilder::new(
                nostr::Kind::Custom(KIND_STREAM_MESSAGE_EDIT as u16),
                "owner adds the mention",
            )
            .tags([
                nostr::Tag::parse(["e", &original.id.to_hex()]).unwrap(),
                nostr::Tag::parse(["p", &agent]).unwrap(),
                nostr::Tag::parse(["h", &channel_id.to_string()]).unwrap(),
            ])
            .sign_with_keys(&owner_keys)
            .unwrap()
        } else {
            original.clone()
        };
        let binding = crate::config::BoundSession {
            channel_id: if case == "foreign-channel" {
                Uuid::new_v4()
            } else {
                channel_id
            },
            thread_root: if matches!(case, "top-level" | "top-level-edit" | "unresolved-edit") {
                original.id.to_hex()
            } else {
                root
            },
        };
        let (gate_rest, gate_server) =
            crate::author_gate_tests::nip11_server(serde_json::json!({})).await;
        let response = if case == "unresolved-edit" {
            serde_json::json!([])
        } else {
            serde_json::json!([original])
        };
        let (query_rest, query_server) = crate::author_gate_tests::nip11_server(response).await;
        let channel_info = crate::pool::ChannelInfoResolver::new(
            std::collections::HashMap::from([(
                channel_id,
                relay::ChannelInfo {
                    name: "bound setup".into(),
                    channel_type: if case == "dm" { "dm" } else { "stream" }.into(),
                    description: None,
                },
            )]),
            gate_rest.clone(),
        );
        let mut gate = InboundAuthorGate::connect(&gate_rest, &agent, "bound setup test").await;
        assert!(setup_listener_admits(&event, &agent));
        let event_id = event.id;
        let authorized = authorize_setup_listener_event(
            &mut gate,
            relay::BuzzEvent {
                connection_generation: 0,
                channel_id,
                event,
            },
            &crate::config::RespondTo::Owner,
            &HashSet::new(),
            &OwnerCache::new(Some(owner)),
            &channel_info,
            &gate_rest,
        )
        .await
        .expect("owner is authorized");
        let (publisher, mut published) = RelayEventPublisher::test_pair();
        let payload = SetupPayload {
            agent_name: "Bound agent".into(),
            agent_pubkey: agent.clone(),
            requirements: vec![],
        };
        let mut nudged = HashSet::new();
        let admitted = nudge_authorized_event(
            authorized,
            &[mentions_rule(crate::config::default_mention_kinds())],
            &agent,
            &mut nudged,
            &publisher,
            &query_rest,
            &agent_keys,
            &payload,
            Some(&binding),
            crate::is_dm_channel(channel_id, &channel_info).await,
        )
        .await;
        assert_eq!(admitted, expected, "{case}");
        assert_eq!(nudged.contains(&event_id), expected, "{case}");
        if expected {
            let nudge = tokio::time::timeout(std::time::Duration::from_secs(5), published.recv())
                .await
                .expect("matching setup nudge reaches the publisher")
                .expect("publisher stays open");
            let scope = crate::scope::SessionScope::derive(
                crate::scope::SessionPolicy::Thread,
                channel_id,
                false,
                &nudge,
            );
            assert!(binding.accepts(&scope), "{case}");
        } else {
            assert!(
                published.try_recv().is_err(),
                "{case}: no out-of-scope nudge"
            );
        }
        gate_server.abort();
        query_server.abort();
    }
}
