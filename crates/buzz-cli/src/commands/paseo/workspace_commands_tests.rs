use super::*;
use axum::{extract::State, routing::post, Json, Router};
use nostr::Keys;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Relay {
    heads: Vec<Event>,
    metadata: Vec<Value>,
    rosters: Vec<Value>,
    writes: Vec<Event>,
    fail_add: bool,
    replacement: Option<Event>,
}
fn value<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| match t.as_slice() {
        [k, v, ..] if k == key => Some(v.as_str()),
        _ => None,
    })
}
async fn query(State(state): State<Arc<Mutex<Relay>>>, Json(filters): Json<Value>) -> Json<Value> {
    let state = state.lock().unwrap();
    let f = &filters[0];
    assert_eq!(f["consistency"], "strong");
    let mut values = match f["kinds"][0].as_u64().unwrap() {
        39000 => state.metadata.clone(),
        39002 => state.rosters.clone(),
        _ => state.heads.iter().map(|e| json!(e)).collect(),
    };
    values.retain(|e| {
        if !f["kinds"].as_array().unwrap().contains(&e["kind"]) {
            return false;
        }
        if let Some(authors) = f["authors"].as_array() {
            if !authors.contains(&e["pubkey"]) {
                return false;
            }
        }
        for key in ["#d", "#p"] {
            if let Some(wanted) = f[key].as_array() {
                if !e["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t[0] == key[1..] && wanted.contains(&t[1]))
                {
                    return false;
                }
            }
        }
        true
    });
    Json(json!(values))
}
async fn publish_event(
    State(state): State<Arc<Mutex<Relay>>>,
    Json(event): Json<Event>,
) -> Json<Value> {
    event.verify().unwrap();
    let mut state = state.lock().unwrap();
    let accepted = !(state.fail_add && event.kind.as_u16() == 9000);
    if accepted {
        match event.kind.as_u16() {
            30621 => {
                state.heads.retain(|h| {
                    !(h.kind == event.kind
                        && h.pubkey == event.pubkey
                        && h.tags.identifier() == event.tags.identifier())
                });
                state.heads.push(event.clone());
                if let Some(replacement) = state.replacement.take() {
                    state.heads.retain(|head| head.id != event.id);
                    state.heads.push(replacement);
                }
            }
            5 => {
                let coord = value(&event, "a").unwrap();
                state.heads.retain(|h| {
                    format!(
                        "{}:{}:{}",
                        h.kind.as_u16(),
                        h.pubkey,
                        h.tags.identifier().unwrap_or("")
                    ) != coord
                });
            }
            9000 => {
                let channel = value(&event, "h").unwrap();
                let key = value(&event, "p").unwrap();
                let roster = state
                    .rosters
                    .iter_mut()
                    .find(|e| {
                        e["tags"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|t| t[0] == "d" && t[1] == channel)
                    })
                    .unwrap();
                let tags = roster["tags"].as_array_mut().unwrap();
                tags.retain(|tag| !(tag[0] == "p" && tag[1] == key));
                tags.push(json!(["p", key, "", "member"]));
            }
            _ => panic!("unexpected write"),
        }
    }
    let id = event.id;
    state.writes.push(event);
    Json(
        json!({"accepted":accepted,"event_id":id,"message":if accepted {""} else {"blocked: fixture"}}),
    )
}
async fn fixture() -> (BuzzClient, Arc<Mutex<Relay>>, tokio::task::JoinHandle<()>) {
    let owner = Keys::generate();
    let state = Arc::new(Mutex::new(Relay {
        metadata: vec![json!({"kind":39000,"tags":[["d",Uuid::nil().to_string()],["t","stream"]]})],
        rosters: vec![
            json!({"kind":39002,"tags":[["d",Uuid::nil().to_string()],["p",owner.public_key().to_hex(),"","member"]]}),
        ],
        ..Relay::default()
    }));
    let app = Router::new()
        .route("/query", post(query))
        .route("/events", post(publish_event))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        BuzzClient::new(url, owner, None, None).unwrap(),
        state,
        server,
    )
}
fn create(agents: Vec<PublicKey>) -> WorkspaceCmd {
    WorkspaceCmd::Create {
        slug: "scratch".into(),
        name: Some("Scratch".into()),
        channel: Uuid::nil(),
        source: vec![],
        repo: vec![],
        listed: false,
        agent: agents,
    }
}
fn update() -> WorkspaceCmd {
    WorkspaceCmd::Update {
        slug: "scratch".into(),
        name: Some("Renamed".into()),
        channel: None,
        source: vec![],
        clear_sources: false,
        listed: false,
        unlisted: false,
    }
}
#[tokio::test]
async fn create_is_rootless_idempotent_and_adds_live_gateway_once() {
    let (client, state, server) = fixture().await;
    let gateway = Keys::generate().public_key();
    let now = Timestamp::now().as_secs();
    state.lock().unwrap().heads.push(
        buzz_sdk::gateway::Delegation::grant(gateway, now + 3600, now)
            .unwrap()
            .sign_with_keys(client.keys())
            .unwrap(),
    );
    execute(create(vec![]), &client).await.unwrap();
    let again = execute(create(vec![]), &client).await.unwrap();
    assert_eq!(again["unchanged"], true);
    let listed = execute(
        WorkspaceCmd::List {
            owner: Some(client.keys().public_key()),
        },
        &client,
    )
    .await
    .unwrap();
    assert_eq!(listed[0]["directory"], "scratch");
    {
        let state = state.lock().unwrap();
        assert_eq!(
            state
                .writes
                .iter()
                .filter(|e| e.kind.as_u16() == 9000)
                .count(),
            1
        );
        assert_eq!(
            state
                .writes
                .iter()
                .filter(|e| e.kind.as_u16() == 30621)
                .count(),
            1
        );
        let project = state
            .heads
            .iter()
            .find(|e| e.kind.as_u16() == 30621)
            .unwrap();
        assert!(workspaces::marked(project));
        assert_eq!(value(project, "buzz-visibility"), Some("unlisted"));
        assert!(project.tags.iter().all(|t| t.as_slice()[0] != "a"));
    }
    execute(
        WorkspaceCmd::Delete {
            slug: "scratch".into(),
        },
        &client,
    )
    .await
    .unwrap();
    assert_eq!(
        execute(WorkspaceCmd::List { owner: None }, &client)
            .await
            .unwrap(),
        json!([])
    );
    server.abort();
}
#[tokio::test]
async fn failed_membership_is_resumable_and_does_not_publish_project() {
    let (client, state, server) = fixture().await;
    let gateway = Keys::generate().public_key();
    let now = Timestamp::now().as_secs();
    {
        let mut state = state.lock().unwrap();
        state.fail_add = true;
        state.heads.push(
            buzz_sdk::gateway::Delegation::grant(gateway, now + 3600, now)
                .unwrap()
                .sign_with_keys(client.keys())
                .unwrap(),
        );
    }
    assert!(execute(create(vec![]), &client).await.is_err());
    assert!(!state
        .lock()
        .unwrap()
        .writes
        .iter()
        .any(|e| e.kind.as_u16() == 30621));
    state.lock().unwrap().fail_add = false;
    execute(create(vec![]), &client).await.unwrap();
    server.abort();
}
#[tokio::test]
async fn unmarked_projects_are_not_listed_updated_deleted_or_overwritten() {
    let (client, state, server) = fixture().await;
    let tags = vec![
        tag(&["d", "scratch"]).unwrap(),
        tag(&["buzz-channel", &Uuid::nil().to_string()]).unwrap(),
    ];
    state
        .lock()
        .unwrap()
        .heads
        .push(client.sign_event(build("", tags).unwrap()).unwrap());
    assert_eq!(
        execute(WorkspaceCmd::List { owner: None }, &client)
            .await
            .unwrap(),
        json!([])
    );
    for cmd in [
        create(vec![]),
        update(),
        WorkspaceCmd::Delete {
            slug: "scratch".into(),
        },
    ] {
        assert!(execute(cmd, &client).await.is_err());
    }
    assert!(state.lock().unwrap().writes.is_empty());
    server.abort();
}
#[tokio::test]
async fn update_preserves_members_description_content_and_unknown_tags() {
    let (client, state, server) = fixture().await;
    execute(create(vec![]), &client).await.unwrap();
    {
        let mut state = state.lock().unwrap();
        let head = state.heads.pop().unwrap();
        let mut tags: Vec<_> = head.tags.iter().cloned().collect();
        tags.extend([
            tag(&["a", &format!("30617:{}:repo", client.keys().public_key())]).unwrap(),
            tag(&["description", "keep"]).unwrap(),
            tag(&["extension", "keep", "also"]).unwrap(),
        ]);
        state.heads.push(
            client
                .sign_event(build("keep content", tags).unwrap())
                .unwrap(),
        );
    }
    execute(update(), &client).await.unwrap();
    let state = state.lock().unwrap();
    let head = state.heads.last().unwrap();
    assert_eq!(value(head, "name"), Some("Renamed"));
    assert_eq!(value(head, "description"), Some("keep"));
    assert!(value(head, "a").is_some());
    assert_eq!(head.content, "keep content");
    assert!(head
        .tags
        .iter()
        .any(|t| t.as_slice() == ["extension", "keep", "also"]));
    server.abort();
}
#[tokio::test]
async fn warns_for_missing_agent_and_over_64_channels() {
    let (client, state, server) = fixture().await;
    let agent = Keys::generate().public_key();
    for _ in 0..65 {
        state.lock().unwrap().rosters.push(
            json!({"kind":39002,"tags":[["d",Uuid::new_v4().to_string()],["p",agent.to_hex(),"","member"]]}),
        );
    }
    let response = execute(create(vec![agent]), &client).await.unwrap();
    assert_eq!(response["warnings"].as_array().unwrap().len(), 2);
    server.abort();
}
#[tokio::test]
async fn rejects_agent_writes_and_invalid_channel_before_side_effects() {
    let (client, state, server) = fixture().await;
    let agent = Keys::generate();
    let auth = buzz_sdk::nip_oa::compute_auth_tag(client.keys(), &agent.public_key(), "").unwrap();
    let agent_client = BuzzClient::new(
        client.relay_url().into(),
        agent,
        Some(buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap()),
        None,
    )
    .unwrap();
    for cmd in [
        create(vec![]),
        update(),
        WorkspaceCmd::Delete {
            slug: "scratch".into(),
        },
    ] {
        assert!(matches!(
            execute(cmd, &agent_client).await,
            Err(CliError::Auth(_))
        ));
    }
    for kind in ["dm", "system", "unknown"] {
        state.lock().unwrap().metadata[0]["tags"][1][1] = json!(kind);
        assert!(execute(create(vec![]), &client).await.is_err());
    }
    assert!(state.lock().unwrap().writes.is_empty());
    server.abort();
}

#[tokio::test]
async fn update_normalizes_lookup_but_preserves_channel_tag() {
    let (client, state, server) = fixture().await;
    execute(create(vec![]), &client).await.unwrap();
    {
        let mut state = state.lock().unwrap();
        let head = state.heads.pop().unwrap();
        let mut tags: Vec<_> = head
            .tags
            .iter()
            .filter(|t| t.as_slice()[0] != "buzz-channel")
            .cloned()
            .collect();
        tags.push(tag(&["buzz-channel", "00000000000000000000000000000000"]).unwrap());
        state
            .heads
            .push(client.sign_event(build("", tags).unwrap()).unwrap());
    }
    execute(update(), &client).await.unwrap();
    assert_eq!(
        value(state.lock().unwrap().heads.last().unwrap(), "buzz-channel"),
        Some("00000000000000000000000000000000")
    );
    server.abort();
}

#[tokio::test]
async fn non_linkable_slugs_create_without_a_broken_deep_link() {
    for slug in ["review notes", "é", "x&owner=other", &"x".repeat(65)] {
        let (client, _, server) = fixture().await;
        let mut command = create(vec![]);
        if let WorkspaceCmd::Create { slug: value, .. } = &mut command {
            *value = slug.into();
        }
        let result = execute(command, &client).await.unwrap();
        assert!(result.get("link").is_none());
        assert!(workspaces::directory_name(slug).starts_with('_'));
        server.abort();
    }
}

#[tokio::test]
async fn content_only_competing_head_is_a_conflict() {
    let (client, state, server) = fixture().await;
    execute(create(vec![]), &client).await.unwrap();
    {
        let mut state = state.lock().unwrap();
        let head = state.heads.last().unwrap();
        let tags = update_tags(head, Some("Renamed"), None, &[], false, false, false).unwrap();
        let replacement = client
            .sign_event(
                build("concurrent content", tags)
                    .unwrap()
                    .custom_created_at(Timestamp::from(head.created_at.as_secs() + 2)),
            )
            .unwrap();
        state.replacement = Some(replacement);
    }
    assert!(matches!(
        execute(update(), &client).await,
        Err(CliError::Conflict(_))
    ));
    server.abort();
}

#[tokio::test]
async fn create_promotes_guest_gateway_to_posting_member() {
    let (client, state, server) = fixture().await;
    let gateway = Keys::generate().public_key();
    let now = Timestamp::now().as_secs();
    {
        let mut state = state.lock().unwrap();
        state.heads.push(
            buzz_sdk::gateway::Delegation::grant(gateway, now + 3600, now)
                .unwrap()
                .sign_with_keys(client.keys())
                .unwrap(),
        );
        state.rosters[0]["tags"]
            .as_array_mut()
            .unwrap()
            .push(json!(["p", gateway, "", "guest"]));
    }
    execute(create(vec![]), &client).await.unwrap();
    execute(create(vec![]), &client).await.unwrap();
    assert_eq!(
        state
            .lock()
            .unwrap()
            .writes
            .iter()
            .filter(|event| event.kind.as_u16() == 9000)
            .count(),
        1
    );
    server.abort();
}
