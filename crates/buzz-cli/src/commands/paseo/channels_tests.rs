use super::*;
use axum::{extract::State, routing::post, Json, Router};
use nostr::{Event, EventBuilder, Keys, Kind, Tag};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn channel(name: &str, channel_type: &str, private: bool, archived: bool) -> Channel {
    Channel {
        id: Uuid::new_v4(),
        name: name.into(),
        channel_type: channel_type.into(),
        private,
        archived,
    }
}

#[test]
fn plan_shares_member_channels_and_never_dms_or_exclusions() {
    let general = channel("general", "stream", false, false);
    let leadership = channel("leadership", "stream", true, false);
    let design = channel("design", "forum", true, false);
    let joined = channel("agents", "stream", false, false);
    let all = vec![
        general.clone(),
        leadership.clone(),
        design.clone(),
        joined.clone(),
        channel("alice-taylor", "dm", true, false),
        channel("system", "system", true, false),
        channel("future", "canvas", false, false),
        channel("old", "stream", false, true),
    ];
    let gateway_in = HashSet::from([joined.id]);
    let workspaces = all.iter().map(|c| c.id).collect();
    let plan = plan(
        all.clone(),
        &gateway_in,
        &workspaces,
        &[leadership.id.to_string()],
        None,
    )
    .unwrap();
    assert_eq!(plan.add, vec![design, general.clone()]);
    assert_eq!(plan.present, vec![joined]);
    assert_eq!(plan.excluded, vec![leadership]);
    assert_eq!(plan.skipped, 4);
    // Rows: an exclusion must name one of the owner's channels.
    for (entry, ok) in [
        ("#general", true),
        ("genral", false),
        ("alice-taylor", true),
    ] {
        let result = super::plan(
            all.clone(),
            &gateway_in,
            &workspaces,
            &[entry.to_string()],
            None,
        );
        assert_eq!(result.is_ok(), ok, "{entry}");
    }
    // A team channel that a later import creates may be excluded before it
    // exists, and stays excluded once it does: the pending name never drops
    // the exclusion.
    let asgard = channel("Asgard", "stream", true, false);
    let pending = ["Asgard".to_string()];
    assert!(super::plan(all.clone(), &gateway_in, &workspaces, &pending, None).is_err());
    let absent = super::plan(
        all.clone(),
        &gateway_in,
        &workspaces,
        &pending,
        Some("Asgard"),
    )
    .unwrap();
    assert!(absent.excluded.is_empty());
    let mut with_asgard = all.clone();
    with_asgard.push(asgard.clone());
    let present = super::plan(
        with_asgard,
        &gateway_in,
        &workspaces,
        &pending,
        Some("Asgard"),
    )
    .unwrap();
    assert_eq!(present.excluded, vec![asgard]);
    assert!(present.add.iter().all(|c| c.name != "Asgard"));
    // Excluding a DM by name never makes it shareable.
    let plan = super::plan(
        all.clone(),
        &gateway_in,
        &workspaces,
        &["alice-taylor".into()],
        None,
    )
    .unwrap();
    assert!(plan.add.iter().all(|c| c.channel_type != "dm") && plan.excluded.is_empty());
}

#[test]
fn channel_parse_reads_relay_metadata_tags() {
    let id = Uuid::new_v4();
    let event = json!({"tags": [
        ["d", id], ["name", "design"], ["private"], ["restricted"],
        ["t", "forum"], ["t", "stream"], ["archived", "true"],
    ]});
    assert_eq!(
        Channel::parse(&event),
        Some(Channel {
            id,
            name: "design".into(),
            channel_type: "forum".into(),
            private: true,
            archived: true,
        })
    );
    // No type tag: unknown, never shared.
    assert_eq!(Channel::parse(&json!({"tags": [["d", id]]})), None);
}

/// A relay that applies channel creation, member adds and grants, refusing
/// adds to `refused` channels.
#[derive(Default)]
struct Relay {
    admitted: Vec<String>,
    projects: Vec<Event>,
    /// channel id -> (kind:39000 tags, roster)
    channels: HashMap<Uuid, (Vec<Vec<String>>, HashSet<String>)>,
    refused: HashSet<Uuid>,
    writes: Vec<Event>,
}

fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [key, value, ..] if key == name => Some(value.as_str()),
        _ => None,
    })
}

async fn query(State(relay): State<Arc<Mutex<Relay>>>, Json(filters): Json<Value>) -> Json<Value> {
    let relay = relay.lock().unwrap();
    let filter = &filters[0];
    let strings = |key: &str| -> Vec<String> {
        filter[key]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let events: Vec<Value> = match filter["kinds"][0].as_u64() {
        Some(13534) => vec![json!({"kind": 13534, "tags": relay
            .admitted
            .iter()
            .map(|key| json!(["member", key, "member"]))
            .collect::<Vec<_>>()})],
        Some(30621) => {
            let authors = strings("authors");
            relay
                .projects
                .iter()
                .filter(|event| authors.contains(&event.pubkey.to_hex()))
                .map(|event| serde_json::to_value(event).unwrap())
                .collect()
        }
        Some(39002) => {
            let wanted = strings("#p");
            relay
                .channels
                .iter()
                .filter(|(_, (_, roster))| wanted.iter().any(|key| roster.contains(key)))
                .map(|(id, (_, roster))| {
                    let mut tags = vec![json!(["d", id])];
                    tags.extend(roster.iter().map(|key| json!(["p", key])));
                    json!({"kind": 39002, "tags": tags})
                })
                .collect()
        }
        Some(39000) => {
            let wanted = strings("#d");
            relay
                .channels
                .iter()
                .filter(|(id, _)| wanted.contains(&id.to_string()))
                .map(|(id, (tags, _))| {
                    let mut all = vec![vec!["d".to_string(), id.to_string()]];
                    all.extend(tags.iter().cloned());
                    json!({"kind": 39000, "tags": all})
                })
                .collect()
        }
        // Grant heads: none yet, so grants start at the current time.
        _ => Vec::new(),
    };
    Json(json!(events))
}

async fn submit(State(relay): State<Arc<Mutex<Relay>>>, Json(event): Json<Event>) -> Json<Value> {
    event.verify().unwrap();
    let mut relay = relay.lock().unwrap();
    let id = event.id;
    let channel = tag_value(&event, "h").and_then(|h| Uuid::parse_str(h).ok());
    let accepted = match (event.kind.as_u16(), channel) {
        (9007, Some(channel)) => {
            let tags = vec![
                vec!["name".into(), tag_value(&event, "name").unwrap().into()],
                vec![tag_value(&event, "visibility")
                    .unwrap()
                    .replace("open", "public")],
                vec![
                    "t".into(),
                    tag_value(&event, "channel_type").unwrap().into(),
                ],
            ];
            relay
                .channels
                .insert(channel, (tags, HashSet::from([event.pubkey.to_hex()])));
            true
        }
        (9000, Some(channel)) if !relay.refused.contains(&channel) => {
            assert_eq!(tag_value(&event, "role"), Some("member"));
            let target = tag_value(&event, "p").unwrap().to_owned();
            relay.channels.get_mut(&channel).unwrap().1.insert(target);
            true
        }
        (9000, _) => false,
        (kind, _) => kind as u32 == buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION,
    };
    relay.writes.push(event);
    Json(
        json!({"event_id": id, "accepted": accepted, "message": if accepted { "" } else { "blocked: refused" }}),
    )
}

fn names(report: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = report[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn share_waits_for_admission_then_shares_and_reruns_idempotently() {
    let owner = Keys::generate();
    let gateway = Keys::generate().public_key();
    let me = owner.public_key().to_hex();
    let other = Keys::generate().public_key().to_hex();
    let relay = Arc::new(Mutex::new(Relay::default()));
    let mut ids = HashMap::new();
    for (name, visibility, channel_type, members) in [
        ("general", "public", "stream", vec![me.clone()]),
        ("leadership", "private", "stream", vec![me.clone()]),
        ("design", "private", "forum", vec![me.clone()]),
        ("broken", "public", "stream", vec![me.clone()]),
        ("dm", "private", "dm", vec![me.clone(), other.clone()]),
        ("not-mine", "private", "stream", vec![other.clone()]),
    ] {
        let id = Uuid::new_v4();
        let tags = vec![
            vec!["name".into(), name.into()],
            vec![visibility.into()],
            vec!["t".into(), channel_type.into()],
        ];
        relay
            .lock()
            .unwrap()
            .channels
            .insert(id, (tags, members.into_iter().collect()));
        if name != "leadership" {
            relay
                .lock()
                .unwrap()
                .projects
                .push(workspace(&owner, name, id));
        }
        ids.insert(name, id);
    }
    relay.lock().unwrap().refused.insert(ids["broken"]);
    let app = Router::new()
        .route("/query", post(query))
        .route("/events", post(submit))
        .with_state(relay.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = BuzzClient::new(url, owner, None, None).unwrap();
    let exclude = ["leadership".to_string()];

    // Not admitted: nothing shared, and the operator command is reported.
    // A community with some members publishes a list.
    relay.lock().unwrap().admitted.push(me.clone());
    let pending = share(&client, gateway, &exclude, None).await.unwrap();
    assert_eq!(pending["admitted"], false);
    assert!(pending["admin_command"]
        .as_str()
        .unwrap()
        .contains(&gateway.to_hex()));
    assert!(relay.lock().unwrap().writes.is_empty());

    relay.lock().unwrap().admitted.push(gateway.to_hex());
    let report = share(&client, gateway, &exclude, None).await.unwrap();
    assert_eq!(names(&report, "added"), ["design", "general"]);
    assert_eq!(names(&report, "failed"), ["broken"]);
    assert_eq!(names(&report, "excluded"), ["leadership"]);
    assert_eq!(report["skipped"], 1);
    {
        let relay = relay.lock().unwrap();
        let has_gateway = |name: &str| relay.channels[&ids[name]].1.contains(&gateway.to_hex());
        assert!(has_gateway("general") && has_gateway("design"));
        assert!(!has_gateway("leadership") && !has_gateway("dm") && !has_gateway("not-mine"));
    }

    // Excluding a channel the gateway already joined reports it as still
    // shared rather than silently claiming it is excluded.
    let writes_now = relay.lock().unwrap().writes.len();
    let later = share(
        &client,
        gateway,
        &["general".to_string(), "leadership".to_string()],
        None,
    )
    .await
    .unwrap();
    assert_eq!(names(&later, "excluded_but_present"), ["general"]);
    assert_eq!(
        relay.lock().unwrap().writes.len(),
        writes_now + 1,
        "only the retry of broken"
    );
    relay.lock().unwrap().refused.insert(ids["broken"]);

    // After the cause is fixed, a rerun shares only what is missing.
    relay.lock().unwrap().refused.clear();
    let writes = relay.lock().unwrap().writes.len();
    let rerun = share(&client, gateway, &exclude, None).await.unwrap();
    assert_eq!(names(&rerun, "added"), ["broken"]);
    assert_eq!(names(&rerun, "already_present"), ["design", "general"]);
    assert_eq!(relay.lock().unwrap().writes.len(), writes + 1);
    server.abort();
}

#[tokio::test]
async fn sharing_only_adds_owner_workspace_channels_and_never_removes_members() {
    for has_workspaces in [false, true] {
        let owner = Keys::generate();
        let owner_key = owner.public_key();
        let gateway = Keys::generate().public_key();
        let gateway_hex = gateway.to_hex();
        let other = Keys::generate();
        let relay = Arc::new(Mutex::new(Relay::default()));
        relay.lock().unwrap().admitted.push(gateway_hex.clone());
        let mut ids = HashMap::new();
        for (name, kind, archived, already_in, workspace_owner) in [
            ("workspace", "stream", false, false, Some(&owner)),
            ("forum", "forum", false, false, Some(&owner)),
            ("present", "stream", false, true, Some(&owner)),
            ("unrelated", "stream", false, false, None),
            ("unrelated-present", "stream", false, true, None),
            ("other-owner", "stream", false, false, Some(&other)),
            ("dm", "dm", false, false, Some(&owner)),
            ("system", "system", false, false, Some(&owner)),
            ("unknown", "canvas", false, false, Some(&owner)),
            ("archived", "stream", true, false, Some(&owner)),
        ] {
            let id = Uuid::new_v4();
            let tags = vec![
                vec!["name".into(), name.into()],
                vec!["t".into(), kind.into()],
                // The first t tag is the channel type; later tags are labels.
                vec!["t".into(), "project-label".into()],
                vec!["archived".into(), archived.to_string()],
            ];
            let mut members = HashSet::from([owner_key.to_hex()]);
            if already_in {
                members.insert(gateway_hex.clone());
            }
            let mut state = relay.lock().unwrap();
            state.channels.insert(id, (tags, members));
            if has_workspaces {
                if let Some(signer) = workspace_owner {
                    state.projects.push(workspace(signer, name, id));
                }
            }
            ids.insert(name, id);
        }
        let initial = relay.lock().unwrap().channels.clone();
        let app = Router::new()
            .route("/query", post(query))
            .route("/events", post(submit))
            .with_state(relay.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = BuzzClient::new(url, owner, None, None).unwrap();
        let report = share(&client, gateway, &[], None).await.unwrap();
        assert_eq!(
            names(&report, "added"),
            if has_workspaces {
                vec!["forum", "workspace"]
            } else {
                vec![]
            }
        );
        assert_eq!(
            names(&report, "already_present"),
            if has_workspaces {
                vec!["present"]
            } else {
                vec![]
            }
        );
        assert!(names(&report, "failed").is_empty());
        {
            let state = relay.lock().unwrap();
            for (id, (_, before)) in &initial {
                let after = &state.channels[id].1;
                assert!(before.is_subset(after), "no membership is removed");
                if !has_workspaces || ![ids["workspace"], ids["forum"]].contains(id) {
                    assert_eq!(before, after, "non-target roster stays untouched");
                }
            }
            assert_eq!(state.writes.len(), if has_workspaces { 2 } else { 0 });
            for event in &state.writes {
                assert_eq!(
                    event.kind.as_u16(),
                    9000,
                    "only membership adds, never 30180/30181"
                );
                assert_eq!(event.pubkey, owner_key);
                assert_eq!(tag_value(event, "p"), Some(gateway_hex.as_str()));
                assert_eq!(tag_value(event, "role"), Some("member"));
            }
        }
        let writes = relay.lock().unwrap().writes.len();
        let again = share(&client, gateway, &[], None).await.unwrap();
        assert!(names(&again, "added").is_empty());
        assert_eq!(
            relay.lock().unwrap().writes.len(),
            writes,
            "rerun is idempotent"
        );
        // Setup renews this global delegation separately from membership.
        super::super::super::gateway::grant(
            &client,
            gateway,
            buzz_sdk::gateway::MAX_DELEGATION_SECS,
        )
        .await
        .unwrap();
        let state = relay.lock().unwrap();
        let delegation = state.writes.last().unwrap();
        assert_eq!(delegation.kind.as_u16(), 30181);
        assert!(tag_value(delegation, "h").is_none());
        assert!(tag_value(delegation, "channel").is_none());
        assert!(state.writes.iter().all(|e| e.kind.as_u16() != 30180));
        server.abort();
    }
}

fn workspace(owner: &Keys, slug: &str, channel: Uuid) -> Event {
    EventBuilder::new(Kind::Custom(30621), "")
        .tags([
            Tag::parse(["d", slug]).unwrap(),
            Tag::parse(["buzz-paseo", "workspace"]).unwrap(),
            Tag::parse(["buzz-channel", &channel.to_string()]).unwrap(),
        ])
        .sign_with_keys(owner)
        .unwrap()
}
