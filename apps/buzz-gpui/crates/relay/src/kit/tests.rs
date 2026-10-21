use super::payload::*;
use super::setup::*;
use super::*;
use crate::event::Verifier;
use buzz_identity::Purpose;
use core::prelude::v1::test;

const COMMUNITY: &str = "https://relay.test";

fn key(i: u8) -> String {
    format!("{i:02x}").repeat(32)
}

fn template_value() -> Value {
    json!({"version": 1, "community": COMMUNITY, "deleted": false, "value": {
        "type": "template", "id": "t-1", "name": "Launch", "description": "",
        "teamIds": ["team-a"], "agents": [key(1)], "canvas": "# Goal"
    }})
}

fn signed(identity: &Identity, record: &KitRecord, extra: Vec<Vec<String>>) -> Verified {
    let mut draft = draft(identity, record).unwrap();
    draft.tags.extend(extra);
    let unsigned = identity
        .template(
            Purpose::AppData,
            draft.kind,
            1_700_000_000,
            draft.tags,
            draft.content,
        )
        .unwrap();
    let event = identity.sign(Purpose::AppData, unsigned).unwrap();
    Verifier::default().verify(event).unwrap()
}

#[test]
fn templates_parse_with_their_native_coordinate() {
    let record = parse_kit_record(&template_value(), COMMUNITY).unwrap();
    assert_eq!(
        record.coordinate(),
        "buzz-channel-kit-v1:https%3A%2F%2Frelay.test:template:t-1"
    );
    assert_eq!(record.tag(), KIT_TAG);
    let KitValue::Template(template) = &record.value else {
        panic!("a template")
    };
    assert_eq!(template.lineup().team_ids, ["team-a"]);
    // Round trip through serde keeps the exact fields the native rules allow.
    assert_eq!(
        parse_kit_record(&record.to_json(), COMMUNITY).unwrap(),
        record
    );
}

#[test]
fn records_from_another_community_or_with_bad_selections_are_refused() {
    assert!(parse_kit_record(&template_value(), "https://other.test").is_err());
    let mut extra = template_value();
    extra["value"]["color"] = json!("red");
    // The model ignores unknown fields, as the TypeScript one does; the
    // native cipher boundary refuses them (see the cipher test).
    assert!(
        !parse_kit_record(&extra, COMMUNITY).unwrap().to_json()["value"]
            .as_object()
            .unwrap()
            .contains_key("color")
    );
    let mut bad = template_value();
    bad["value"]["agents"] = json!(["nope"]);
    assert_eq!(
        parse_kit_record(&bad, COMMUNITY).unwrap_err(),
        "Invalid channel recipe selection"
    );
    let mut duplicate = template_value();
    duplicate["value"]["agents"] = json!([key(1), key(1)]);
    assert!(parse_kit_record(&duplicate, COMMUNITY).is_err());
    let mut blank = template_value();
    blank["value"]["name"] = json!("  ");
    assert!(parse_kit_record(&blank, COMMUNITY).is_err());
}

#[test]
fn canvas_and_record_budgets_have_their_messages() {
    let mut large = template_value();
    large["value"]["canvas"] = json!("é".repeat(13 * 1024));
    assert_eq!(
        parse_kit_record(&large, COMMUNITY).unwrap_err(),
        "Canvas is too large (24 KiB maximum)"
    );
    let mut over = template_value();
    over["value"]["canvas"] = json!("x".repeat(17 * 1024));
    assert_eq!(parse_kit_record(&over, COMMUNITY).unwrap_err(), TOO_LARGE);
}

#[test]
fn groups_records_follow_the_me_rules() {
    let channel = "11111111-1111-4111-8111-111111111111";
    let me = json!({"version": 1, "community": COMMUNITY, "deleted": false, "value": {
        "type": "groups", "id": "me",
        "groups": [{"id": "g1", "name": "Work", "defaultTemplateId": "t-1"}],
        "assignments": {channel: "g1"},
        "channels": [channel]
    }});
    let record = parse_kit_record(&me, COMMUNITY).unwrap();
    assert_eq!(record.tag(), ME_KIT_TAG);
    // Personal groups are retired: such a record is no longer a recipe.
    let mut personal = me.clone();
    personal["value"]["id"] = json!("personal");
    assert_eq!(
        parse_kit_record(&personal, COMMUNITY).unwrap_err(),
        "Unsupported channel recipe type"
    );
    let mut stray = me.clone();
    stray["value"]["assignments"] = json!({channel: "missing"});
    assert_eq!(
        parse_kit_record(&stray, COMMUNITY).unwrap_err(),
        "Invalid Me group placement"
    );
    let mut twice = me.clone();
    twice["value"]["groups"] = json!([
        {"id": "g1", "name": "Work", "defaultTemplateId": ""},
        {"id": "g1", "name": "Again", "defaultTemplateId": ""}
    ]);
    assert_eq!(
        parse_kit_record(&twice, COMMUNITY).unwrap_err(),
        "Duplicate Me group"
    );
}

#[test]
fn catalog_heads_skip_retired_personal_groups() {
    let alice = Identity::ephemeral();
    let record = parse_kit_record(&template_value(), COMMUNITY).unwrap();
    let template = signed(&alice, &record, vec![]);
    let mut draft = draft(&alice, &record).unwrap();
    for tag in &mut draft.tags {
        if tag[0] == "d" {
            tag[1] = format!("{KIT_TAG}:https%3A%2F%2Frelay.test:groups:personal");
        }
    }
    let unsigned = alice
        .template(
            Purpose::AppData,
            draft.kind,
            1_700_000_000,
            draft.tags,
            draft.content,
        )
        .unwrap();
    let personal = Verifier::default()
        .verify(alice.sign(Purpose::AppData, unsigned).unwrap())
        .unwrap();
    let events = vec![template.clone(), personal];
    let heads = catalog_heads(&events, COMMUNITY);
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, template.id);
}

#[test]
fn portable_teams_need_version_two_and_a_manifest() {
    let manifest = json!({"version": 1, "owner": key(9), "revision": "11111111-1111-4111-8111-111111111111",
        "digest": key(8), "bytes": 20000, "chunks": 2});
    let team = json!({"version": 2, "community": COMMUNITY, "deleted": false, "value": {
        "type": "team", "id": "team-a", "name": "Squad", "agents": [key(1)], "portable": manifest
    }});
    let record = parse_kit_record(&team, COMMUNITY).unwrap();
    assert_eq!(record.tag(), MANIFEST_TAG);
    let mut v1 = team.clone();
    v1["version"] = json!(1);
    assert_eq!(
        parse_kit_record(&v1, COMMUNITY).unwrap_err(),
        "Portable teams require version 2"
    );
    let mut empty = team.clone();
    empty["value"]["agents"] = json!([]);
    assert_eq!(
        parse_kit_record(&empty, COMMUNITY).unwrap_err(),
        "Unsupported portable recipe"
    );
    let mut wrong = team;
    wrong["value"]["portable"]["chunks"] = json!(3);
    assert_eq!(
        parse_kit_record(&wrong, COMMUNITY).unwrap_err(),
        "Invalid portable team manifest"
    );
}

#[test]
fn the_cipher_round_trips_only_the_viewers_admitted_records() {
    let alice = Identity::ephemeral();
    let record = parse_kit_record(&template_value(), COMMUNITY).unwrap();
    let event = signed(&alice, &record, vec![vec!["client-id".into(), "x".into()]]);
    let entry = decode_entry(&alice, &event, COMMUNITY).unwrap();
    assert_eq!(entry.record, record);
    // Another viewer cannot open it, and an extra tag fails admission.
    let bob = Identity::ephemeral();
    assert!(open(&bob, &event, COMMUNITY).is_err());
    let tagged = signed(&alice, &record, vec![vec!["p".into(), key(2)]]);
    assert!(open(&alice, &tagged, COMMUNITY).is_err());
    // Nothing the native rules refuse is ever encrypted.
    let mut extra = template_value();
    extra["value"]["x"] = json!(1);
    assert!(seal(&alice, &extra, COMMUNITY).is_err());
}

#[test]
fn catalog_heads_keep_the_newest_per_coordinate_in_this_community() {
    let alice = Identity::ephemeral();
    let record = parse_kit_record(&template_value(), COMMUNITY).unwrap();
    let older = signed(&alice, &record, vec![]);
    let draft = draft(&alice, &record).unwrap();
    let newer = {
        let unsigned = alice
            .template(
                Purpose::AppData,
                draft.kind,
                1_700_000_100,
                draft.tags,
                draft.content,
            )
            .unwrap();
        Verifier::default()
            .verify(alice.sign(Purpose::AppData, unsigned).unwrap())
            .unwrap()
    };
    let other = parse_kit_record(
        &json!({"version": 1, "community": "https://other.test", "deleted": false, "value": {
            "type": "team", "id": "x", "name": "X", "agents": []}}),
        "https://other.test",
    )
    .unwrap();
    let foreign = signed(&alice, &other, vec![]);
    let events = vec![older, newer.clone(), foreign];
    let heads = catalog_heads(&events, COMMUNITY);
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, newer.id);
}

#[test]
fn lineups_resolve_teams_and_report_unavailable_members() {
    let team = KitEntry {
        record: KitRecord::new(
            KitValue::Team(Team {
                id: "team-a".into(),
                name: "A".into(),
                agents: vec![key(1), key(2)],
                portable: None,
            }),
            COMMUNITY,
            false,
        ),
        event_id: EventId::from_hex(&"0".repeat(64)).unwrap(),
        created_at: 1,
    };
    let choice = |i: u8| AgentChoice {
        pubkey: key(i),
        name: format!("Agent {i}"),
        avatar: None,
    };
    let lineup = Lineup {
        team_ids: vec!["team-a".into()],
        agents: vec![key(2), key(3)],
        canvas: String::new(),
    };
    let agents = [choice(1), choice(2), choice(3)];
    let resolved = resolve_lineup(&lineup, std::slice::from_ref(&team), &agents).unwrap();
    let keys: Vec<_> = resolved.iter().map(|a| a.pubkey.clone()).collect();
    assert_eq!(keys, [key(2), key(3), key(1)], "distinct, agents first");
    assert!(
        resolve_lineup(&lineup, &[], &agents)
            .unwrap_err()
            .starts_with("A selected team is unavailable (team-a)")
    );
    let missing = resolve_lineup(&lineup, &[team], &agents[..2]).unwrap_err();
    assert!(missing.starts_with("Agent npub1"), "{missing}");
}

/// `team-payload.test.ts`.
#[test]
fn text_manifests_have_exactly_their_six_fields() {
    let manifest = json!({
        "version": 1,
        "owner": key(1),
        "revision": "00000000-0000-4000-8000-000000000000",
        "digest": "cd".repeat(32),
        "bytes": 9,
        "chunks": 1,
    });
    assert_eq!(
        serde_json::to_value(parse_text_manifest(&manifest).unwrap()).unwrap(),
        manifest
    );
    let mut extra = manifest.clone();
    extra["extra"] = json!(1);
    assert!(
        parse_text_manifest(&extra)
            .unwrap_err()
            .contains("manifest")
    );
}

#[test]
fn payload_chunks_are_keyed_by_community_owner_team_revision_and_index() {
    let owner = key(1);
    let revision = "00000000-0000-4000-8000-000000000000";
    assert_eq!(
        payload_coordinate("https://relay.example", &owner, "team", revision, 2),
        format!("buzz-team-payload-v1:https%3A%2F%2Frelay.example:{owner}:team:{revision}:2")
    );
    // A chunk record parses back to itself under that coordinate.
    let chunk = TeamPayload {
        version: 1,
        community: COMMUNITY.into(),
        owner,
        team_id: "team".into(),
        revision: revision.into(),
        index: 2,
        data: "e30=".into(),
    };
    let record = chunk.record();
    assert_eq!(parse_payload_record(&record, COMMUNITY).unwrap(), chunk);
    assert_eq!(
        buzz_desktop::relay::kit::coordinate(&record, COMMUNITY).unwrap(),
        chunk.coordinate()
    );
}

#[test]
fn creation_input_checks_details_and_setup() {
    let channel = crate::lifecycle::NewChannel {
        name: " #launch ".into(),
        description: " ".into(),
        private: true,
        ttl: Some(3600),
        labels: Vec::new(),
    };
    let input = ChannelCreationInput::from_new(&channel, Some(ChannelSetup::default()));
    assert_eq!(input.name, "launch");
    assert_eq!(input.description, None);
    assert_eq!(input.setup, None, "an empty setup is no setup");
    assert!(input.check().is_ok());
    let mut bad = input.clone();
    bad.setup = Some(ChannelSetup {
        template_id: "bad id".into(),
        ..ChannelSetup::default()
    });
    assert!(bad.check().is_err());
    bad.setup = Some(ChannelSetup {
        agents: vec![key(1)],
        canvas: "x".repeat(CANVAS_BYTES + 1),
        ..ChannelSetup::default()
    });
    assert!(bad.check().is_err());
    let mut session = input;
    session.description = Some("Buzz session (x)".into());
    assert!(session.check().is_err());
}

#[test]
fn receipts_round_trip_within_their_budget() {
    let store = crate::Store::in_memory().unwrap();
    let partition = crate::store::Partition {
        origin: COMMUNITY.into(),
        viewer: key(1),
    };
    let input = ChannelCreationInput::from_new(
        &crate::lifecycle::NewChannel {
            name: "launch".into(),
            description: String::new(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        },
        None,
    );
    let mut progress = Progress::new(input);
    Receipts::write(&store, &partition, &progress).unwrap();
    assert!(
        !Receipts::load(&store, &partition)
            .unwrap()
            .admitted(&progress.id)
    );
    progress.created = true;
    Receipts::write(&store, &partition, &progress).unwrap();
    assert!(
        Receipts::load(&store, &partition)
            .unwrap()
            .admitted(&progress.id)
    );
    let mut huge = progress.clone();
    huge.added = (0..2000).map(|_| key(3)).collect();
    assert_eq!(
        Receipts::write(&store, &partition, &huge).unwrap_err(),
        "Channel setup exceeds its local receipt budget"
    );
    Receipts::remove(&store, &partition, &progress.id).unwrap();
    assert!(Receipts::load(&store, &partition).unwrap().0.is_empty());
}

#[test]
fn creations_restore_only_from_this_apps_command_shape() {
    let identity = Identity::ephemeral();
    let id = "11111111-1111-4111-8111-111111111111";
    let channel = crate::lifecycle::NewChannel {
        name: "launch".into(),
        description: "Plans".into(),
        private: false,
        ttl: Some(600),
        labels: Vec::new(),
    };
    let draft = crate::lifecycle::create(&channel, id).unwrap();
    let mut tags = draft.tags;
    tags.push(vec!["client-id".into(), "abc".into()]);
    let op = |tags: Vec<Vec<String>>| crate::outbox::Operation {
        event: identity
            .template(Purpose::Content, draft.kind, 1, tags, String::new())
            .unwrap(),
        signed: None,
        recovery: None,
        acknowledged: false,
        guarded: false,
        delivery: crate::outbox::Delivery::Unknown,
        error: None,
    };
    let pending = pending_creation(&op(tags.clone())).unwrap();
    assert_eq!(pending.id, id);
    assert_eq!(pending.input.description.as_deref(), Some("Plans"));
    assert_eq!(pending.input.ttl_seconds, Some(600));
    let mut reordered = tags.clone();
    reordered.swap(4, 5);
    assert!(pending_creation(&op(reordered)).is_none());
    let mut session = tags.clone();
    session[4][1] = "Buzz session (x)".into();
    assert!(pending_creation(&op(session)).is_none());
    // Label tags close the command (#964).
    let mut labeled = tags.clone();
    labeled.push(vec!["t".into(), "for-you".into()]);
    let pending = pending_creation(&op(labeled.clone())).unwrap();
    assert_eq!(pending.input.labels, ["for-you"]);
    assert_eq!(pending.input.ttl_seconds, Some(600));
    let mut twice = labeled.clone();
    twice.push(vec!["t".into(), "for-you".into()]);
    assert!(pending_creation(&op(twice)).is_none());
    let mut reserved = tags;
    reserved.push(vec!["t".into(), "stream".into()]);
    assert!(pending_creation(&op(reserved)).is_none());
}

fn text_head(owner: &str, bytes: u64) -> Value {
    json!({"version": 1, "community": COMMUNITY, "deleted": false, "value": {
        "type": "team-text", "id": "crew", "owner": owner,
        "manifest": {"version": 1, "owner": owner,
            "revision": "11111111-1111-4111-8111-111111111111",
            "digest": "cd".repeat(32), "bytes": bytes, "chunks": bytes.div_ceil(16 * 1024)}
    }})
}

/// Signs `raw` encrypted as is, so invalid records reach admission.
fn sealed(identity: &Identity, raw: &Value, tags: Vec<Vec<String>>) -> Verified {
    let content = identity.encrypt_to_self(&raw.to_string()).unwrap();
    let unsigned = identity
        .template(Purpose::AppData, 30078, 1_700_000_000, tags, content)
        .unwrap();
    Verifier::default()
        .verify(identity.sign(Purpose::AppData, unsigned).unwrap())
        .unwrap()
}

/// buzz-app's shared corpus (`team-text-admission-contract.json`), run
/// against the port's decode of the viewer's own heads.
#[test]
fn team_text_heads_match_the_shared_admission_corpus() {
    let identity = Identity::ephemeral();
    let corpus =
        include_str!("team-text-admission-contract.json").replace("OWNER", &identity.pubkey_hex());
    let cases: Vec<Value> = serde_json::from_str(&corpus).unwrap();
    assert!(cases.len() > 10);
    for case in cases {
        let tags = serde_json::from_value(case["tags"].clone()).unwrap();
        let event = sealed(&identity, &case["record"], tags);
        assert_eq!(
            open(&identity, &event, COMMUNITY).is_ok(),
            case["accepted"] == true,
            "{}",
            case["name"]
        );
    }
}

#[test]
fn team_text_records_parse_with_their_own_coordinate_and_owner() {
    let identity = Identity::ephemeral();
    let owner = identity.pubkey_hex();
    let live = parse_text_record(&text_head(&owner, 3), COMMUNITY).unwrap();
    assert_eq!(
        live.coordinate(),
        format!("buzz-team-text-v1:https%3A%2F%2Frelay.test:{owner}:crew")
    );
    assert_eq!(live.to_json(), text_head(&owner, 3));
    let tombstone = TextRecord::new(COMMUNITY, "crew", &owner, None);
    assert!(tombstone.deleted);
    assert_eq!(
        parse_private_record(&tombstone.to_json(), COMMUNITY).unwrap(),
        PrivateRecord::Text(tombstone)
    );
    let mut bad = vec![text_head(&owner, 2), text_head(&owner, 4 + 128 * 1024)];
    let mut mismatched = text_head(&owner, 3);
    mismatched["value"]["manifest"]["owner"] = json!(key(9));
    bad.push(mismatched);
    let mut live_null = text_head(&owner, 3);
    live_null["value"]["manifest"] = Value::Null;
    bad.push(live_null);
    let mut dead_manifest = text_head(&owner, 3);
    dead_manifest["deleted"] = json!(true);
    bad.push(dead_manifest);
    let mut inline = text_head(&owner, 3);
    inline["value"]["text"] = json!("inline");
    bad.push(inline);
    let mut v2 = text_head(&owner, 3);
    v2["version"] = json!(2);
    bad.push(v2);
    for raw in &bad {
        assert!(parse_text_record(raw, COMMUNITY).is_err(), "{raw}");
    }
    assert!(parse_text_record(&text_head(&owner, 3), "https://other.test").is_err());
    // Another owner's head, even self-consistent, is never opened.
    let other = text_head(&key(9), 3);
    let d = native::coordinate(&other, COMMUNITY).unwrap();
    let event = sealed(
        &identity,
        &other,
        vec![vec!["d".into(), d], vec!["t".into(), TEXT_TAG.into()]],
    );
    assert!(open(&identity, &event, COMMUNITY).is_err());
}
