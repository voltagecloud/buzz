use super::*;
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

fn record(keys: &Keys, slug: &str, time: u64, sources: &[String]) -> Event {
    let mut tags = vec![
        vec!["d".into(), slug.into()],
        vec!["buzz-paseo".into(), "workspace".into()],
        vec!["buzz-channel".into(), Uuid::nil().to_string()],
    ];
    tags.extend(
        sources
            .iter()
            .map(|s| vec!["buzz-source".into(), s.clone()]),
    );
    signed(keys, tags, time)
}
fn signed(keys: &Keys, tags: Vec<Vec<String>>, time: u64) -> Event {
    EventBuilder::new(Kind::Custom(KIND_PROJECT as u16), "")
        .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
        .custom_created_at(Timestamp::from(time))
        .sign_with_keys(keys)
        .unwrap()
}
fn metadata(kind: &str, archived: bool) -> Vec<Value> {
    vec![
        json!({"kind":39000,"tags":[["d",Uuid::nil().to_string()],["t",kind],["archived",archived.to_string()]]}),
    ]
}
fn change(keys: &Keys, event: &Event, key: &str, values: &[Vec<&str>]) -> Event {
    let mut tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .filter(|t| t.as_slice()[0] != key)
        .map(|t| t.as_slice().to_vec())
        .collect();
    tags.extend(
        values
            .iter()
            .map(|v| v.iter().map(|s| s.to_string()).collect()),
    );
    signed(keys, tags, event.created_at.as_secs() + 1)
}
#[test]
fn rootless_shared_channels_and_visibility() {
    let key = Keys::generate();
    let a = record(&key, "a", 2, &[]);
    let b = change(
        &key,
        &record(&key, "b", 1, &[]),
        "buzz-visibility",
        &[vec!["buzz-visibility", "unlisted"]],
    );
    let selection = select(key.public_key(), &[b, a], &metadata("stream", false));
    assert_eq!(
        selection
            .workspaces
            .iter()
            .map(|w| w.slug.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(selection.skipped.is_empty());
}
#[test]
fn channel_allowlist() {
    let key = Keys::generate();
    let event = record(&key, "x", 1, &[]);
    for (kind, archived, count) in [
        ("stream", false, 1),
        ("forum", false, 1),
        ("dm", false, 0),
        ("system", false, 0),
        ("unknown", false, 0),
        ("stream", true, 0),
    ] {
        assert_eq!(
            select(
                key.public_key(),
                std::slice::from_ref(&event),
                &metadata(kind, archived)
            )
            .workspaces
            .len(),
            count
        );
    }
    assert!(select(key.public_key(), &[event], &[])
        .workspaces
        .is_empty());
}
#[test]
fn signature_owner_marker_and_replacement_validation() {
    let key = Keys::generate();
    let event = record(&key, "x", 1, &[]);
    let meta = metadata("stream", false);
    assert!(select(
        Keys::generate().public_key(),
        std::slice::from_ref(&event),
        &meta
    )
    .workspaces
    .is_empty());
    let mut invalid = event.clone();
    invalid.content = "tampered".into();
    assert!(select(key.public_key(), &[invalid], &meta)
        .workspaces
        .is_empty());
    for (tag, values) in [
        ("buzz-paseo", vec![]),
        (
            "buzz-paseo",
            vec![
                vec!["buzz-paseo", "workspace"],
                vec!["buzz-paseo", "workspace"],
            ],
        ),
        ("buzz-paseo", vec![vec!["buzz-paseo", "wrong"]]),
        ("buzz-channel", vec![]),
        ("buzz-channel", vec![vec!["buzz-channel", "bad"]]),
        (
            "buzz-channel",
            vec![
                vec!["buzz-channel", "00000000-0000-0000-0000-000000000000"],
                vec!["buzz-channel", "00000000-0000-0000-0000-000000000000"],
            ],
        ),
        ("auth", vec![vec!["auth", "owner"]]),
        ("d", vec![vec!["d", ""]]),
    ] {
        let bad = change(&key, &event, tag, &values);
        let values = if tag == "d" {
            vec![bad]
        } else {
            vec![event.clone(), bad]
        };
        assert!(
            select(key.public_key(), &values, &meta)
                .workspaces
                .is_empty(),
            "{tag}"
        );
    }
}
#[test]
fn source_rules_and_caps_with_overlap() {
    for source in [
        "http://github.com/o/r",
        "https://github.com/o/r/",
        "https://github.com/o/r?x",
        "https://github.com/o/..",
        "https://github.com/o/r#x",
        "https://user@github.com/o/r",
        "https://github.com/o/%2e",
        "https://github.com/o/r/extra",
    ] {
        assert!(validate_source(source).is_err(), "{source}");
    }
    assert!(validate_source(&format!("https://github.com/o/{}", "x".repeat(256))).is_err());
    let key = Keys::generate();
    let sources: Vec<_> = (0..65)
        .map(|i| format!("https://github.com/o/r{i}"))
        .collect();
    let mut events: Vec<_> = (0..4)
        .map(|i| {
            record(
                &key,
                &format!("w{i}"),
                100 - i as u64,
                &sources[i * 16..(i + 1) * 16],
            )
        })
        .collect();
    events.push(record(&key, "overlap", 90, &sources[..16]));
    events.push(record(&key, "overflow", 89, &sources[60..]));
    events.push(record(&key, "seventeen", 88, &sources[..17]));
    let selection = select(key.public_key(), &events, &metadata("stream", false));
    assert_eq!(selection.workspaces.len(), 5);
    assert_eq!(selection.skipped.len(), 2);
    let events: Vec<_> = (0..34)
        .rev()
        .map(|i| record(&key, &format!("w{i:02}"), 1, &[]))
        .collect();
    let selection = select(key.public_key(), &events, &metadata("stream", false));
    assert_eq!(selection.workspaces.len(), 32);
    assert_eq!(selection.workspaces[31].slug, "w31");
}
#[test]
fn directory_grammar_and_forced_collision() {
    for slug in ["x", "0", "a.b_c-d", &"a".repeat(64)] {
        assert_eq!(directory_name(slug), slug);
    }
    for slug in ["", "_x", "Upper", "../x", ".", "..", "é", &"a".repeat(65)] {
        let hashed = directory_name(slug);
        assert!(hashed.starts_with('_'));
        assert_eq!(hashed.len(), 17);
        assert_ne!(directory_name(&hashed), hashed);
    }
    let key = Keys::generate();
    let events = vec![record(&key, "a", 1, &[]), record(&key, "b", 2, &[])];
    let selection = select_with_directory(
        key.public_key(),
        &events,
        &metadata("stream", false),
        |_| "collision".into(),
    );
    assert!(selection.workspaces.is_empty());
    assert_eq!(selection.skipped.len(), 2);
}

#[tokio::test]
async fn load_normalizes_channel_uuid_and_uses_strong_reads() {
    use axum::{routing::post, Json, Router};
    let key = Keys::generate();
    let event = change(
        &key,
        &record(&key, "x", 1, &[]),
        "buzz-channel",
        &[vec!["buzz-channel", "00000000000000000000000000000000"]],
    );
    let app = Router::new().route(
        "/query",
        post(move |Json(filters): Json<Value>| {
            let event = event.clone();
            async move {
                assert_eq!(filters[0]["consistency"], "strong");
                if filters[0]["kinds"][0] == KIND_PROJECT {
                    Json(json!([event]))
                } else {
                    assert_eq!(filters[0]["#d"], json!([Uuid::nil().to_string()]));
                    Json(json!(metadata("stream", false)))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = BuzzClient::new(url, key.clone(), None, None).unwrap();
    assert_eq!(
        load(&client, key.public_key())
            .await
            .unwrap()
            .workspaces
            .len(),
        1
    );
    server.abort();
}

#[test]
fn channel_labels_do_not_change_the_first_type_tag() {
    let key = Keys::generate();
    let event = record(&key, "labeled", 1, &[]);
    let mut meta = metadata("stream", false);
    meta[0]["tags"]
        .as_array_mut()
        .unwrap()
        .push(json!(["t", "urgent"]));
    assert_eq!(
        select(key.public_key(), &[event], &meta).workspaces.len(),
        1
    );
}

#[test]
fn metadata_ids_normalize_deduplicate_and_sort() {
    let keys = Keys::generate();
    let base = record(&keys, "x", 1, &[]);
    let ids = [
        "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
        "00000000-0000-0000-0000-000000000000",
        "ffffffffffffffffffffffffffffffff",
        "invalid",
    ];
    let mut events: Vec<_> = ids
        .iter()
        .map(|id| change(&keys, &base, "buzz-channel", &[vec!["buzz-channel", id]]))
        .collect();
    events.push(change(&keys, &base, "buzz-channel", &[]));
    assert_eq!(
        metadata_ids(&events),
        [
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff"
        ]
    );
    assert!(metadata_ids(&[]).is_empty());
}

#[test]
fn reader_retains_hash_fallback_for_external_slugs() {
    let keys = Keys::generate();
    let event = record(&keys, "Payments Review", 1, &[]);
    let selection = select(keys.public_key(), &[event], &metadata("stream", false));
    assert!(selection.skipped.is_empty());
    assert_eq!(selection.workspaces.len(), 1);
    assert_eq!(
        selection.workspaces[0].directory,
        directory_name("Payments Review")
    );
    assert!(selection.workspaces[0].directory.starts_with('_'));
}
