use super::*;
use crate::testing::{CHANNEL, Signer, T0, key};

struct Channel {
    signer: Signer,
    meta: Vec<Vec<String>>,
    admins: Vec<(String, &'static str)>,
    members: Vec<(String, &'static str)>,
}

impl Channel {
    fn new() -> Self {
        let signer = Signer::new();
        let viewer = signer.viewer_hex();
        Self {
            signer,
            meta: vec![
                vec!["name".into(), "general".into()],
                vec!["about".into(), "Chat".into()],
                vec!["public".into()],
                vec!["t".into(), "stream".into()],
            ],
            admins: vec![(viewer.clone(), "owner")],
            members: vec![(viewer, "owner"), (key(2), "member")],
        }
    }

    fn events(&self) -> Vec<Verified> {
        let d = vec!["d".to_owned(), CHANNEL.to_owned()];
        let meta: Vec<Vec<String>> = std::iter::once(d.clone())
            .chain(self.meta.iter().cloned())
            .collect();
        let admins: Vec<Vec<String>> = std::iter::once(d.clone())
            .chain(
                self.admins
                    .iter()
                    .map(|(k, r)| vec!["p".to_owned(), k.clone(), r.to_string()]),
            )
            .collect();
        let roster: Vec<Vec<String>> = std::iter::once(d)
            .chain(
                self.members
                    .iter()
                    .map(|(k, r)| vec!["p".to_owned(), k.clone(), String::new(), r.to_string()]),
            )
            .collect();
        let sign = |kind, rows: &[Vec<String>]| {
            let rows: Vec<Vec<&str>> = rows
                .iter()
                .map(|r| r.iter().map(String::as_str).collect())
                .collect();
            let refs: Vec<&[&str]> = rows.iter().map(Vec::as_slice).collect();
            self.signer.relay(kind, T0, &refs)
        };
        vec![
            sign(kinds::CHANNEL_METADATA, &meta),
            sign(kinds::CHANNEL_ADMINS, &admins),
            sign(kinds::CHANNEL_MEMBERS, &roster),
        ]
    }

    fn state(&self) -> Result<ChannelState, LifecycleError> {
        ChannelState::parse(&self.events(), CHANNEL, &self.signer.relay_hex())
    }
}

#[test]
fn parses_a_consistent_channel() {
    let channel = Channel::new();
    let state = channel.state().unwrap();
    let viewer = channel.signer.viewer_hex();
    assert_eq!(state.name, "general");
    assert_eq!(state.about, "Chat");
    assert_eq!(state.channel_type, StateType::Stream);
    assert_eq!(state.visibility, Some(Visibility::Public));
    assert_eq!(state.role(&viewer), Some(MemberRole::Owner));
    assert_eq!(state.role(&key(2)), Some(MemberRole::Member));
    assert!(state.can_manage(&viewer));
    assert!(state.can_manage_member(&viewer, &key(2)));
    assert!(!state.can_manage_member(&viewer, &viewer));
    assert!(!state.can_manage(&key(2)));
}

#[test]
fn reads_the_type_from_the_first_t_tag_of_a_labeled_channel() {
    // The relay writes the type first, then labels (NIP-CL).
    let mut channel = Channel::new();
    channel.meta.push(vec!["t".into(), "workspace".into()]);
    channel.meta.push(vec!["t".into(), "team:core".into()]);
    let state = channel.state().unwrap();
    assert_eq!(state.channel_type, StateType::Stream);
    assert_eq!(
        state.role(&key(2)),
        Some(MemberRole::Member),
        "member authority reads the same state"
    );

    let mut channel = Channel::new();
    channel.meta.retain(|tag| tag[0] != "t");
    channel
        .meta
        .push(vec!["t".into(), "stream".into(), "extra".into()]);
    channel.meta.push(vec!["t".into(), "workspace".into()]);
    assert_eq!(channel.state().unwrap_err().0, "Malformed channel t state");
}

#[test]
fn refuses_inconsistent_roles() {
    let mut channel = Channel::new();
    channel.admins.push((key(2), "admin"));
    assert!(channel.state().unwrap_err().0.contains("inconsistent"));

    let mut channel = Channel::new();
    channel.admins.clear();
    assert!(channel.state().is_err(), "an owner missing from 39001");
}

#[test]
fn refuses_state_signed_by_someone_else() {
    let channel = Channel::new();
    let other = Signer::new();
    assert!(ChannelState::parse(&channel.events(), CHANNEL, &other.relay_hex()).is_err());
}

#[test]
fn the_last_owner_cannot_leave() {
    let channel = Channel::new();
    let state = channel.state().unwrap();
    let viewer = channel.signer.viewer_hex();
    let owner = state.lifecycle(&viewer).unwrap();
    assert!(!owner.can_leave);
    assert!(owner.can_archive && owner.can_delete && !owner.can_hide);
    assert_eq!(
        owner.leave_reason.as_deref(),
        Some("Transfer ownership before leaving the channel.")
    );
    let member = state.lifecycle(&key(2)).unwrap();
    assert!(member.can_leave && !member.can_archive);
    assert!(state.lifecycle(&key(9)).is_err(), "not a member");
}

#[test]
fn archived_channels_offer_unarchive_and_no_administration() {
    let mut channel = Channel::new();
    channel.meta.push(vec!["archived".into(), "true".into()]);
    let state = channel.state().unwrap();
    let viewer = channel.signer.viewer_hex();
    let permissions = state.lifecycle(&viewer).unwrap();
    assert!(permissions.can_unarchive && !permissions.can_archive);
    assert!(!state.can_manage(&viewer));
    assert!(!state.details(&viewer).unwrap().can_edit);
}

#[test]
fn dms_can_only_be_hidden() {
    let mut channel = Channel::new();
    channel.meta = vec![
        vec!["name".into(), "DM".into()],
        vec!["private".into()],
        vec!["hidden".into()],
        vec!["t".into(), "dm".into()],
    ];
    let viewer = channel.signer.viewer_hex();
    channel.admins.clear();
    channel.members = vec![(viewer.clone(), "member"), (key(2), "member")];
    let state = channel.state().unwrap();
    let permissions = state.lifecycle(&viewer).unwrap();
    assert!(permissions.can_hide);
    assert!(!permissions.can_leave && !permissions.can_archive);
    assert!(!state.can_manage(&viewer));
}

#[test]
fn builds_lifecycle_templates() {
    let archive = template(Action::Archive, CHANNEL, 5).unwrap();
    assert_eq!(archive.kind, 9002);
    assert_eq!(
        archive.tags,
        vec![vec!["h", CHANNEL], vec!["archived", "true"]]
    );
    let join = template(Action::Join, CHANNEL, 5).unwrap();
    assert_eq!((join.kind, join.tags.len()), (9021, 1));
    assert_eq!(template(Action::Leave, CHANNEL, 5).unwrap().kind, 9022);
    assert_eq!(template(Action::Hide, CHANNEL, 5).unwrap().kind, 41012);
    assert!(template(Action::Delete, "general", 5).is_err());
    assert!(template(Action::Join, "general", 5).is_err());
}

#[test]
fn confirms_join_leave_and_archive() {
    let signer = Signer::new();
    let relay = signer.relay_hex();
    let viewer = signer.viewer_hex();
    let roster = signer.relay(
        kinds::CHANNEL_MEMBERS,
        T0,
        &[&["d", CHANNEL], &["p", &viewer]],
    );
    let joined = [roster];
    assert!(confirmed(Action::Join, &joined, &relay, &viewer, CHANNEL));
    assert!(!confirmed(Action::Leave, &joined, &relay, &viewer, CHANNEL));
    assert!(confirmed(Action::Leave, &[], &relay, &viewer, CHANNEL));
    let archived = signer.relay(
        kinds::CHANNEL_METADATA,
        T0,
        &[&["d", CHANNEL], &["archived", "true"]],
    );
    assert!(confirmed(
        Action::Archive,
        std::slice::from_ref(&archived),
        &relay,
        &viewer,
        CHANNEL
    ));
    assert!(!confirmed(
        Action::Unarchive,
        std::slice::from_ref(&archived),
        &relay,
        &viewer,
        CHANNEL
    ));
    // A deleted channel's metadata is gone.
    assert!(confirmed(Action::Delete, &[], &relay, &viewer, CHANNEL));
    assert!(!confirmed(
        Action::Delete,
        &[archived],
        &relay,
        &viewer,
        CHANNEL
    ));
}

#[test]
fn only_owners_delete_and_owner_agents_extend_that() {
    let mut channel = Channel::new();
    let viewer = channel.signer.viewer_hex();
    let delete = template(Action::Delete, CHANNEL, 5).unwrap();
    assert_eq!(
        (delete.kind, delete.tags.clone()),
        (9008, vec![vec!["h".to_owned(), CHANNEL.to_owned()]])
    );
    let confirm = confirm_filter(Action::Delete, "r", &viewer, CHANNEL);
    assert_eq!(confirm["kinds"], json!([39000]));
    assert_eq!(confirm["consistency"], json!("strong"));

    // An admin archives but does not delete; the owner is an agent.
    let agent = key(3);
    channel.admins = vec![(agent.clone(), "owner"), (viewer.clone(), "admin")];
    channel.members = vec![(agent.clone(), "owner"), (viewer.clone(), "admin")];
    let state = channel.state().unwrap();
    assert_eq!(state.owner_keys(), vec![agent.clone()]);
    let mut admin = state.lifecycle(&viewer).unwrap();
    assert!(admin.can_archive && !admin.can_delete);
    assert!(!Action::Delete.allowed(&admin));
    assert!(admin.wants_owner_evidence(false));
    admin.grant_owned_agent(false);
    assert!(admin.can_delete && admin.can_archive && !admin.can_unarchive);
    assert!(Action::Delete.allowed(&admin));
    let filters = owner_profile_filters(&state.owner_keys());
    assert_eq!(filters.len(), 1);
    assert_eq!(filters[0]["authors"], json!([agent]));

    // Archived: only Unarchive can be granted; deleting is not offered.
    channel.meta.push(vec!["archived".into(), "true".into()]);
    let mut archived = channel.state().unwrap().lifecycle(&viewer).unwrap();
    assert!(archived.can_unarchive && !archived.wants_owner_evidence(true));
    archived.grant_owned_agent(true);
    assert!(!archived.can_delete && archived.can_unarchive);
}

#[test]
fn join_needs_open_unarchived_metadata() {
    let signer = Signer::new();
    let relay = signer.relay_hex();
    let viewer = signer.viewer_hex();
    let open = signer.relay(
        kinds::CHANNEL_METADATA,
        T0,
        &[&["d", CHANNEL], &["public"], &["t", "stream"]],
    );
    assert_eq!(
        join_ready(std::slice::from_ref(&open), &relay, &viewer, CHANNEL),
        Ok(false)
    );
    let roster = signer.relay(
        kinds::CHANNEL_MEMBERS,
        T0,
        &[&["d", CHANNEL], &["p", &viewer]],
    );
    assert_eq!(
        join_ready(&[open, roster], &relay, &viewer, CHANNEL),
        Ok(true)
    );
    let private = signer.relay(
        kinds::CHANNEL_METADATA,
        T0,
        &[&["d", CHANNEL], &["private"]],
    );
    assert!(join_ready(&[private], &relay, &viewer, CHANNEL).is_err());
    assert!(join_ready(&[], &relay, &viewer, CHANNEL).is_err());
}

#[test]
fn creates_a_stream_in_relay_tag_order() {
    let draft = create(
        &NewChannel {
            name: "  #release-notes ".into(),
            description: " Notes ".into(),
            private: true,
            ttl: None,
            labels: Vec::new(),
        },
        CHANNEL,
    )
    .unwrap();
    assert_eq!(draft.kind, 9007);
    assert_eq!(
        draft.tags,
        vec![
            vec!["h", CHANNEL],
            vec!["name", "release-notes"],
            vec!["visibility", "private"],
            vec!["channel_type", "stream"],
            vec!["about", "Notes"],
        ]
    );
    let bare = NewChannel {
        name: "x".into(),
        description: String::new(),
        private: false,
        ttl: None,
        labels: Vec::new(),
    };
    let draft = create(&bare, CHANNEL).unwrap();
    assert_eq!(draft.tags.len(), 4, "no empty about");
    assert_eq!(draft.tags[2], vec!["visibility", "open"]);
    let empty = NewChannel {
        name: " # ".into(),
        ..bare.clone()
    };
    assert!(create(&empty, CHANNEL).is_err());
    let long = NewChannel {
        name: "x".repeat(121),
        ..bare.clone()
    };
    assert!(create(&long, CHANNEL).is_err());
    let session = NewChannel {
        description: "Buzz session (buzz.sessions/v1)".into(),
        ..bare.clone()
    };
    assert!(create(&session, CHANNEL).is_err());
    let temporary = NewChannel {
        description: "Notes".into(),
        ttl: Some(DEFAULT_TEMPORARY_TTL),
        labels: Vec::new(),
        ..bare.clone()
    };
    let draft = create(&temporary, CHANNEL).unwrap();
    assert_eq!(draft.tags[4], vec!["about", "Notes"]);
    assert_eq!(draft.tags[5], vec!["ttl", "604800"], "ttl last");
    for ttl in [0, 2_147_483_648] {
        let invalid = NewChannel {
            ttl: Some(ttl),
            labels: Vec::new(),
            ..bare.clone()
        };
        assert!(create(&invalid, CHANNEL).is_err());
    }
}

#[test]
fn draft_errors_match_the_field_feedback() {
    assert!(!draft_errors("general", "", None).any());
    assert!(!draft_errors(&"x".repeat(120), &"y".repeat(1000), Some(1)).any());
    let errors = draft_errors("", &"y".repeat(1001), Some(0));
    assert_eq!(errors.name, Some(NAME_ERROR));
    assert_eq!(errors.description, Some(ABOUT_LENGTH_ERROR));
    assert_eq!(errors.lifetime, Some(TTL_ERROR));
    // Code points, not bytes or UTF-16 units.
    assert!(draft_errors(&"é".repeat(120), "", None).name.is_none());
    assert!(draft_errors(&"😀".repeat(121), "", None).name.is_some());
    assert!(draft_errors("#general", "", None).name.is_some());
    assert_eq!(
        draft_errors("general", "a Buzz session (x)", None).description,
        Some(ABOUT_SESSION_ERROR)
    );
    assert!(
        draft_errors("g", "", Some(2_147_483_648))
            .lifetime
            .is_some()
    );
}

#[test]
fn details_edits_send_only_changed_visibility_and_ttl() {
    let base = DetailsDraft {
        name: "general".into(),
        description: "Chat".into(),
        private: false,
        ttl: None,
        labels: None,
    };
    let renamed = DetailsDraft {
        name: "lobby".into(),
        ..base.clone()
    };
    let event = edit_details(CHANNEL, &renamed, &base, 1).unwrap();
    assert_eq!(
        event.tags,
        vec![
            vec!["h", CHANNEL],
            vec!["name", "lobby"],
            vec!["about", "Chat"]
        ]
    );
    let private = DetailsDraft {
        private: true,
        ttl: Some(60),
        ..base.clone()
    };
    let event = edit_details(CHANNEL, &private, &base, 1).unwrap();
    assert_eq!(event.tags[3], vec!["visibility", "private"]);
    assert_eq!(event.tags[4], vec!["ttl", "60"]);
    let cleared = edit_details(CHANNEL, &base, &private, 1).unwrap();
    assert_eq!(cleared.tags[3], vec!["visibility", "open"]);
    assert_eq!(cleared.tags[4], vec!["ttl", ""]);
    let leading = DetailsDraft {
        name: "#x".into(),
        ..base.clone()
    };
    assert!(edit_details(CHANNEL, &leading, &base, 1).is_err());
}

#[test]
fn details_edits_replace_or_clear_labels_only_when_they_change() {
    let mut channel = Channel::new();
    channel.meta.push(vec!["t".into(), "for-you".into()]);
    let state = channel.state().unwrap();
    assert_eq!(state.labels, ["for-you"]);
    let base = state.details(&channel.signer.viewer_hex()).unwrap().draft;
    assert_eq!(base.labels.as_deref(), Some(&["for-you".to_owned()][..]));
    // Unchanged (or unset) labels send no `t` tags: the relay keeps them.
    let same = edit_details(CHANNEL, &base, &base, 1).unwrap();
    assert!(same.tags.iter().all(|tag| tag[0] != "t"));
    let unset = DetailsDraft {
        labels: None,
        ..base.clone()
    };
    assert!(details_confirmed(&state, &unset));
    let relabeled = DetailsDraft {
        labels: Some(vec!["b".into(), "a".into(), "b".into()]),
        ..base.clone()
    };
    let event = edit_details(CHANNEL, &relabeled, &base, 1).unwrap();
    assert_eq!(event.tags[3..], [vec!["t", "b"], vec!["t", "a"]]);
    assert!(!details_confirmed(&state, &relabeled));
    assert!(labels_unsupported(&relabeled, &base, false));
    assert!(!labels_unsupported(&relabeled, &base, true));
    assert!(!labels_unsupported(&base, &base, false));
    let cleared = DetailsDraft {
        labels: Some(Vec::new()),
        ..base.clone()
    };
    let event = edit_details(CHANNEL, &cleared, &base, 1).unwrap();
    assert_eq!(event.tags[3..], [vec!["t", ""]]);
    let bad = DetailsDraft {
        labels: Some(vec!["stream".into()]),
        ..base
    };
    assert!(edit_details(CHANNEL, &bad, &unset, 1).is_err());
}

#[test]
fn creation_appends_label_tags_last() {
    let channel = NewChannel {
        name: "ops".into(),
        description: String::new(),
        private: false,
        ttl: Some(60),
        labels: vec!["for-you".into(), "for-you".into()],
    };
    let draft = create(&channel, CHANNEL).unwrap();
    assert_eq!(draft.tags[4], vec!["ttl", "60"]);
    assert_eq!(draft.tags[5..], [vec!["t", "for-you"]]);
    let reserved = NewChannel {
        labels: vec!["dm".into()],
        ..channel
    };
    assert!(create(&reserved, CHANNEL).is_err());
}

#[test]
fn details_confirmation_compares_every_field() {
    let channel = Channel::new();
    let state = channel.state().unwrap();
    let details = state.details(&channel.signer.viewer_hex()).unwrap();
    assert!(details.can_edit);
    assert_eq!(details.version, state.version);
    assert!(details_confirmed(&state, &details.draft));
    let changed = DetailsDraft {
        description: "Other".into(),
        ..details.draft
    };
    assert!(!details_confirmed(&state, &changed));
}

#[test]
fn reads_the_hidden_dm_list() {
    let signer = Signer::new();
    let relay = signer.relay_hex();
    let viewer = signer.viewer_hex();
    let list = signer.relay(
        kinds::HIDDEN_DMS,
        T0,
        &[&["d", &viewer], &["p", &viewer], &["h", CHANNEL]],
    );
    assert_eq!(
        hidden_dms(std::slice::from_ref(&list), &relay, &viewer),
        Some(vec![CHANNEL.to_owned()])
    );
    assert!(confirmed(Action::Hide, &[list], &relay, &viewer, CHANNEL));
    let bad = signer.relay(
        kinds::HIDDEN_DMS,
        T0 + 1,
        &[&["d", &viewer], &["p", &viewer], &["h", "not-a-uuid"]],
    );
    assert_eq!(hidden_dms(&[bad], &relay, &viewer), None);
    assert_eq!(hidden_dms(&[], &relay, &viewer), None);
}

#[test]
fn sessions_rename_with_their_name_alone() {
    let base = DetailsDraft {
        name: "Fix the build".into(),
        description: crate::discovery::SESSION_CHANNEL_DESCRIPTION.into(),
        private: true,
        ttl: None,
        labels: None,
    };
    let renamed = DetailsDraft {
        name: "Fix the tests".into(),
        ..base.clone()
    };
    let event = edit_details(CHANNEL, &renamed, &base, 1).unwrap();
    assert_eq!(
        event.tags,
        vec![vec!["h", CHANNEL], vec!["name", "Fix the tests"]]
    );
    let opened = DetailsDraft {
        private: false,
        ..renamed
    };
    assert_eq!(
        edit_details(CHANNEL, &opened, &base, 1).unwrap_err().0,
        "Only the name of a session can be changed here."
    );
}

#[test]
fn invalid_visibility_lists_are_errors() {
    let signer = Signer::new();
    let relay = signer.relay_hex();
    let viewer = signer.viewer_hex();
    let bad = signer.relay(
        kinds::HIDDEN_DMS,
        T0,
        &[&["d", &viewer], &["p", &viewer], &["h", "not-a-uuid"]],
    );
    assert_eq!(
        visibility_record(&[bad], &relay, &viewer).unwrap_err().0,
        "Invalid hidden DM"
    );
    let other = signer.relay(kinds::HIDDEN_DMS, T0, &[&["d", &viewer], &["p", &relay]]);
    assert_eq!(
        visibility_record(&[other], &relay, &viewer).unwrap_err().0,
        "Invalid DM visibility snapshot"
    );
    assert_eq!(visibility_record(&[], &relay, &viewer), Ok(None));
}

#[test]
fn reads_admin_keys_from_a_relay_signed_39001_only() {
    let signer = Signer::new();
    let relay = signer.relay_hex();
    let viewer = signer.viewer_hex();
    let admin = key(2);
    let admins = signer.relay(
        kinds::CHANNEL_ADMINS,
        T0,
        &[
            &["d", CHANNEL],
            &["p", &viewer, "owner"],
            &["p", &admin, "admin"],
        ],
    );
    let expected: HashSet<PublicKey> = [&viewer, &admin]
        .into_iter()
        .map(|key| PublicKey::from_hex(key).unwrap())
        .collect();
    assert_eq!(channel_admins(&admins, CHANNEL, &relay), Some(expected));
    assert_eq!(channel_admins(&admins, "other", &relay), None);
    let forged = signer.sign(
        &signer.viewer,
        kinds::CHANNEL_ADMINS,
        T0,
        "",
        &[&["d", CHANNEL], &["p", &viewer, "owner"]],
    );
    assert_eq!(channel_admins(&forged, CHANNEL, &relay), None);
    let member = signer.relay(
        kinds::CHANNEL_ADMINS,
        T0,
        &[&["d", CHANNEL], &["p", &key(3), "member"]],
    );
    assert_eq!(channel_admins(&member, CHANNEL, &relay), None);
    let dm = signer.relay(kinds::CHANNEL_ADMINS, T0, &[&["d", CHANNEL]]);
    assert_eq!(channel_admins(&dm, CHANNEL, &relay), Some(HashSet::new()));
}
