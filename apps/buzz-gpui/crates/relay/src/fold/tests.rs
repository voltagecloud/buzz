//! Ported from buzz-app's `fold.test.ts`, `message-order.test.ts`,
//! `reactions.test.ts`, `message-edit.test.ts` and `threads.test.ts`.

use super::*;
use crate::event::Verifier;
use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use serde_json::json;

const CHANNEL: &str = "chan-1";

fn signed(keys: &Keys, kind: u16, content: &str, created_at: u64, tags: &[&[&str]]) -> Verified {
    let tags = tags
        .iter()
        .map(|tag| Tag::parse(tag.iter().copied()).unwrap());
    let event = EventBuilder::new(Kind::from_u16(kind), content)
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(created_at))
        .finalize(keys)
        .unwrap();
    Verifier::new(1).verify(event).unwrap()
}

fn message(
    keys: &Keys,
    channel: &str,
    content: &str,
    created_at: u64,
    extra: &[&[&str]],
) -> Verified {
    let h = ["h", channel];
    let mut tags: Vec<&[&str]> = vec![&h];
    tags.extend_from_slice(extra);
    signed(keys, kinds::CHAT, content, created_at, &tags)
}

fn summary(relay: &Keys, row: &Verified, body: serde_json::Value, created_at: u64) -> Verified {
    let id = row.id_hex();
    signed(
        relay,
        kinds::THREAD_SUMMARY,
        &body.to_string(),
        created_at,
        &[&["e", &id], &["d", &id], &["h", CHANNEL]],
    )
}

struct People {
    relay: Keys,
    alice: Keys,
    bob: Keys,
}

fn people() -> People {
    People {
        relay: Keys::generate(),
        alice: Keys::generate(),
        bob: Keys::generate(),
    }
}

fn fold(relay: &Keys, events: &[Verified]) -> Vec<Arc<MessageRow>> {
    fold_messages(
        CHANNEL,
        &relay.public_key(),
        events,
        &FoldOptions::default(),
    )
}

fn ids(rows: &[Arc<MessageRow>]) -> Vec<EventId> {
    rows.iter().map(|row| row.id).collect()
}

fn attachment(url: &str, kind: AttachmentKind) -> Attachment {
    Attachment {
        url: url.to_owned(),
        kind,
        mime: None,
        size: None,
        name: None,
        duration: None,
        waveform: None,
        voice_note: false,
        dimensions: None,
        blurhash: None,
        preview_url: None,
        insecure: false,
    }
}

fn file(url: &str, mime: &str, name: Option<&str>) -> Attachment {
    Attachment {
        mime: Some(mime.to_owned()),
        name: name.map(str::to_owned),
        ..attachment(url, AttachmentKind::File)
    }
}

fn parse(event: &Verified, images: &[&str]) -> Vec<Attachment> {
    let images: Vec<String> = images.iter().map(|s| s.to_string()).collect();
    parse_attachments(event, &images, &HashMap::new())
}

#[test]
fn preserves_diff_bytes_and_metadata_without_projection() {
    let p = people();
    let content =
        "@@ -1 +1 @@\n-![old](https://example.com/old.png)\n+<script>alert(1)</script>  \n";
    let event = signed(
        &p.alice,
        kinds::DIFF,
        content,
        1,
        &[
            &["h", CHANNEL],
            &["file", "README.md"],
            &["repo", "javascript:alert(1)"],
            &["commit", "abcdef0"],
            &["description", "Raw <code>"],
            &["truncated", "true"],
        ],
    );
    let rows = fold(&p.relay, &[event]);
    assert_eq!(rows[0].content, content);
    assert!(rows[0].attachments.is_empty());
    assert!(rows[0].attachment_seams.is_empty());
    assert_eq!(
        rows[0].diff,
        Some(DiffMeta {
            file_path: Some("README.md".into()),
            repo_url: Some("javascript:alert(1)".into()),
            commit_sha: Some("abcdef0".into()),
            description: Some("Raw <code>".into()),
            truncated: true,
        })
    );
    let untagged = signed(&p.alice, kinds::DIFF, content, 1, &[&["h", CHANNEL]]);
    assert_eq!(
        fold(&p.relay, &[untagged])[0].diff,
        Some(DiffMeta::default())
    );
}

#[test]
fn orders_rows_and_excludes_other_channels_and_non_broadcast_replies() {
    let p = people();
    let a = message(&p.alice, CHANNEL, "a", 20, &[]);
    let b = message(&p.bob, CHANNEL, "b", 10, &[]);
    let same = message(&p.bob, CHANNEL, "c", 10, &[]);
    let other = message(&p.alice, "chan-2", "elsewhere", 30, &[]);
    let a_id = a.id_hex();
    let reply = message(&p.bob, CHANNEL, "reply", 25, &[&["e", &a_id, "", "reply"]]);
    let broadcast = message(
        &p.bob,
        CHANNEL,
        "broadcast",
        26,
        &[&["e", &a_id, "", "reply"], &["broadcast", "1"]],
    );
    let root_only = message(
        &p.bob,
        CHANNEL,
        "root marker only",
        27,
        &[&["e", &a_id, "", "root"]],
    );
    let rows = fold(
        &p.relay,
        &[
            a.clone(),
            b.clone(),
            same.clone(),
            other,
            reply,
            broadcast.clone(),
            root_only.clone(),
        ],
    );
    let mut ten = [b.id, same.id];
    ten.sort_by_key(|id| id.to_hex());
    assert_eq!(
        ids(&rows),
        [ten[0], ten[1], a.id, broadcast.id, root_only.id]
    );

    let find = |id| rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(find(broadcast.id).thread_root_id, Some(a.id));
    assert_eq!(find(broadcast.id).reply_parent_id, Some(a.id));
    assert_eq!(find(root_only.id).reply_parent_id, None);
    assert_eq!(find(root_only.id).thread_root_id, None);
    assert_eq!(find(a.id).thread_root_id, None);

    let with_replies = fold_messages(
        CHANNEL,
        &p.relay.public_key(),
        &[a, b, same],
        &FoldOptions {
            include_replies: true,
            ..Default::default()
        },
    );
    assert_eq!(with_replies.len(), 3);
}

#[test]
fn applies_author_deletes_latest_author_edit_reactions_and_mentions() {
    let p = people();
    let bob_hex = p.bob.public_key().to_hex();
    let a = message(
        &p.alice,
        CHANNEL,
        "hello @bob",
        10,
        &[&["p", &bob_hex], &["p", "not-a-key"]],
    );
    let gone = message(&p.alice, CHANNEL, "deleted", 11, &[]);
    let spoofed = message(&p.alice, CHANNEL, "still here", 12, &[]);
    let (a_id, gone_id, spoofed_id) = (a.id_hex(), gone.id_hex(), spoofed.id_hex());
    let events = [
        a.clone(),
        gone,
        spoofed.clone(),
        signed(&p.alice, kinds::DELETION, "", 12, &[&["e", &gone_id]]),
        signed(&p.bob, kinds::DELETION, "", 12, &[&["e", &spoofed_id]]),
        signed(&p.alice, kinds::EDIT, "edit one", 13, &[&["e", &a_id]]),
        signed(&p.alice, kinds::EDIT, "edit two", 14, &[&["e", &a_id]]),
        signed(&p.bob, kinds::EDIT, "not my message", 15, &[&["e", &a_id]]),
        signed(&p.bob, kinds::REACTION, "👍", 15, &[&["e", &a_id]]),
        signed(&p.alice, kinds::REACTION, "👍", 16, &[&["e", &a_id]]),
        signed(&p.bob, kinds::REACTION, "🎉", 16, &[&["e", &a_id]]),
    ];
    let rows = fold(&p.relay, &events);
    assert_eq!(ids(&rows), [a.id, spoofed.id]);
    let row = &rows[0];
    assert_eq!(row.content, "edit two");
    assert!(row.edited);
    assert_eq!(row.mentions, [p.bob.public_key()]);
    let contents: Vec<&str> = row.reactions.iter().map(|g| g.content.as_str()).collect();
    assert_eq!(contents, ["👍", "🎉"]);
    assert_eq!(row.reactions[0].events.len(), 2);
    assert_eq!(row.reply_count, 0);
    assert!(row.participants.is_empty());
}

#[test]
fn attributes_relay_signed_output_only_with_a_signing_authority() {
    let p = people();
    let (alice_hex, bob_hex) = (p.alice.public_key().to_hex(), p.bob.public_key().to_hex());
    let workflow: [&[&str]; 4] = [
        &["p", &alice_hex],
        &["h", CHANNEL],
        &["buzz:workflow", "true"],
        &["buzz:workflow-owner", &alice_hex],
    ];
    let run = signed(&p.relay, kinds::CHAT, "scheduled", 10, &workflow);
    let actor = signed(
        &p.relay,
        kinds::CHAT,
        "acted",
        11,
        &[&["h", CHANNEL], &["actor", &bob_hex], &["p", &alice_hex]],
    );
    let not_leading = signed(
        &p.relay,
        kinds::CHAT,
        "system",
        12,
        &[&["h", CHANNEL], &["p", &alice_hex]],
    );
    let spoofed = signed(&p.bob, kinds::CHAT, "spoof", 13, &workflow);
    let (run_id, not_leading_id) = (run.id_hex(), not_leading.id_hex());
    let events = [
        run,
        actor,
        not_leading,
        spoofed,
        signed(
            &p.alice,
            kinds::EDIT,
            "edited by owner",
            14,
            &[&["e", &run_id]],
        ),
        signed(
            &p.bob,
            kinds::EDIT,
            "not the owner",
            15,
            &[&["e", &not_leading_id]],
        ),
    ];
    let authority = FoldOptions {
        signing_authority: Some(p.relay.public_key()),
        ..Default::default()
    };
    let rows = fold_messages(CHANNEL, &p.relay.public_key(), &events, &authority);
    let summary: Vec<(PublicKey, &str)> = rows
        .iter()
        .map(|r| (r.author, r.content.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            (p.alice.public_key(), "edited by owner"),
            (p.bob.public_key(), "acted"),
            (p.relay.public_key(), "system"),
            (p.bob.public_key(), "spoof"),
        ]
    );
    assert_eq!(rows[0].signer, Some(p.relay.public_key()));
    assert_eq!(rows[0].workflow_owner, Some(p.alice.public_key()));
    assert_eq!(rows[2].signer, None);
    assert_eq!(rows[3].workflow_owner, None);

    // Without an explicit NIP-11 self the signer stays the author and gains
    // no delegated edit authority.
    let rows = fold(&p.relay, &events);
    let summary: Vec<(PublicKey, &str)> = rows
        .iter()
        .map(|r| (r.author, r.content.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            (p.relay.public_key(), "scheduled"),
            (p.relay.public_key(), "acted"),
            (p.relay.public_key(), "system"),
            (p.bob.public_key(), "spoof"),
        ]
    );
    assert_eq!(rows[0].workflow_owner, None);
}

#[test]
fn applies_owner_message_deletes_but_not_owner_deletes_of_reactions() {
    let p = people();
    let agent_message = message(&p.alice, CHANNEL, "agent message", 10, &[]);
    let message_id = agent_message.id_hex();
    let agent_reaction = signed(&p.alice, kinds::REACTION, "👍", 11, &[&["e", &message_id]]);
    let reaction_id = agent_reaction.id_hex();
    let delete_reaction = signed(
        &p.bob,
        kinds::DELETION,
        "",
        12,
        &[&["e", &reaction_id], &["k", "7"]],
    );
    let delete_message = signed(
        &p.bob,
        kinds::DELETION,
        "",
        13,
        &[&["e", &message_id], &["k", "9"]],
    );
    let rows = fold(
        &p.relay,
        &[
            agent_message.clone(),
            agent_reaction.clone(),
            delete_reaction,
        ],
    );
    assert_eq!(rows[0].reactions.len(), 1);
    assert!(fold(&p.relay, &[agent_message, agent_reaction, delete_message]).is_empty());
}

#[test]
fn applies_a_moderator_delete_only_from_a_channel_admin() {
    let p = people();
    let target = message(&p.alice, CHANNEL, "spam", 10, &[]);
    let target_id = target.id_hex();
    let moderation = signed(
        &p.bob,
        kinds::MODERATOR_DELETE,
        "",
        11,
        &[&["h", CHANNEL], &["e", &target_id]],
    );
    let elsewhere = signed(
        &p.bob,
        kinds::MODERATOR_DELETE,
        "",
        11,
        &[&["h", "chan-2"], &["e", &target_id]],
    );
    let events = [target.clone(), moderation];
    // Parity: a 9005 from someone other than the author is ignored.
    assert_eq!(fold(&p.relay, &events).len(), 1);
    let admins = FoldOptions {
        channel_admins: HashSet::from([p.bob.public_key()]),
        ..Default::default()
    };
    let relay = p.relay.public_key();
    assert!(fold_messages(CHANNEL, &relay, &events, &admins).is_empty());
    assert_eq!(
        fold_messages(CHANNEL, &relay, &[target, elsewhere], &admins).len(),
        1
    );
}

#[test]
fn reads_relay_signed_thread_summaries_only() {
    let p = people();
    let a = message(&p.alice, CHANNEL, "a", 10, &[]);
    let b = message(&p.alice, CHANNEL, "b", 11, &[]);
    let c = message(&p.alice, CHANNEL, "c", 12, &[]);
    let bob_hex = p.bob.public_key().to_hex();
    let c_id = c.id_hex();
    let rows = fold(
        &p.relay,
        &[
            a.clone(),
            b.clone(),
            c.clone(),
            summary(
                &p.relay,
                &a,
                json!({"reply_count": 4, "descendant_count": 7, "participants": [bob_hex, bob_hex, "junk"]}),
                20,
            ),
            summary(
                &p.bob,
                &b,
                json!({"reply_count": 99, "participants": []}),
                20,
            ),
            signed(
                &p.relay,
                kinds::THREAD_SUMMARY,
                "{not json",
                20,
                &[&["e", &c_id], &["d", &c_id], &["h", CHANNEL]],
            ),
        ],
    );
    let counts: Vec<(u64, Vec<PublicKey>)> = rows
        .iter()
        .map(|row| (row.reply_count, row.participants.clone()))
        .collect();
    assert_eq!(
        counts,
        [(7, vec![p.bob.public_key()]), (0, vec![]), (0, vec![])]
    );
}

#[test]
fn falls_back_to_a_validated_direct_count() {
    let p = people();
    let root = message(&p.alice, CHANNEL, "root", 10, &[]);
    for (total, expected) in [
        (json!(0), 0),
        (json!(null), 1),
        (json!(-1), 1),
        (json!(1.5), 1),
        (json!("3"), 1),
        (json!(4.0), 4),
    ] {
        let body = json!({"reply_count": 1, "descendant_count": total, "participants": []});
        let rows = fold(
            &p.relay,
            &[root.clone(), summary(&p.relay, &root, body, 20)],
        );
        assert_eq!(rows[0].reply_count, expected, "{total}");
    }
}

#[test]
fn newer_summary_wins_and_same_second_ties_go_to_the_lower_id() {
    let p = people();
    let root = message(&p.alice, CHANNEL, "root", 10, &[]);
    let count = |n: u64| json!({"descendant_count": n, "participants": []});
    let old = summary(&p.relay, &root, count(9), 20);
    let new = summary(&p.relay, &root, count(3), 21);
    let rows = fold(&p.relay, &[root.clone(), new.clone(), old.clone()]);
    assert_eq!(rows[0].reply_count, 3);

    let four = summary(&p.relay, &root, count(4), 30);
    let five = summary(&p.relay, &root, count(5), 30);
    let lower = if four.id < five.id { 4 } else { 5 };
    for events in [
        [root.clone(), four.clone(), five.clone()],
        [root.clone(), five, four],
    ] {
        assert_eq!(fold(&p.relay, &events)[0].reply_count, lower);
    }
}

#[test]
fn leaves_deeply_nested_markdown_unprojected() {
    let p = people();
    let content = format!("{}![deep](https://x.test/deep.png)", "> ".repeat(10_000));
    let rows = fold(&p.relay, &[message(&p.alice, CHANNEL, &content, 10, &[])]);
    assert!(rows[0].content.contains("![deep](https://x.test/deep.png)"));
    assert!(rows[0].attachments.is_empty());
}

#[test]
fn projects_extensionless_markdown_images() {
    let p = people();
    let url = format!("https://relay.test/files/{}", "a".repeat(64));
    let event = message(
        &p.alice,
        CHANNEL,
        &format!("See ![relay image]({url}) now"),
        10,
        &[],
    );
    let rows = fold(&p.relay, std::slice::from_ref(&event));
    assert_eq!(rows[0].content, "See  now");
    assert_eq!(
        rows[0].source_content.as_deref(),
        Some(event.content.as_str())
    );
    assert_eq!(
        rows[0].attachments,
        [attachment(&url, AttachmentKind::Image)]
    );
}

#[test]
fn preserves_code_indentation_when_projecting_images() {
    let p = people();
    let content = "    @Mic\n\n![image](https://x.test/image.png)";
    for kind in [kinds::CHAT, kinds::AGENT_MESSAGE] {
        let body = if kind == kinds::AGENT_MESSAGE {
            json!({ "content": content }).to_string()
        } else {
            content.to_owned()
        };
        let event = signed(&p.alice, kind, &body, 1, &[&["h", CHANNEL]]);
        let rows = fold(&p.relay, &[event]);
        assert_eq!(rows[0].content, "    @Mic");
        assert_eq!(
            rows[0].attachments,
            [attachment(
                "https://x.test/image.png",
                AttachmentKind::Image
            )]
        );
    }
}

#[test]
fn reads_the_audience_from_the_original_events_signed_tag() {
    let p = people();
    let rivet = "b".repeat(64);
    let tagged = |audience: &str| {
        let event = message(
            &p.alice,
            CHANNEL,
            "hi",
            5,
            &[&["p", &rivet], &["audience", audience]],
        );
        (event.clone(), fold(&p.relay, &[event])[0].audience)
    };
    assert_eq!(tagged("everyone").1, None);
    assert_eq!(tagged("unknown").1, None);
    let (original, audience) = tagged("agents");
    assert_eq!(audience, Some(Audience::Agents));
    // An edit carries no audience of its own.
    let edit = signed(
        &p.alice,
        kinds::EDIT,
        "edited",
        6,
        &[&["e", &original.id_hex()]],
    );
    assert_eq!(
        fold(&p.relay, &[original, edit])[0].audience,
        Some(Audience::Agents)
    );
    let plain = message(&p.alice, CHANNEL, "hi", 5, &[]);
    assert_eq!(fold(&p.relay, &[plain])[0].audience, None);
}

#[test]
fn keeps_the_original_kind_hint_independent_of_body_and_edits() {
    let p = people();
    let cases = [
        (kinds::CHAT, "Plain message".to_owned()),
        (
            kinds::CHAT,
            json!({"content": "Envelope-looking prose", "is_agent": true}).to_string(),
        ),
        (
            kinds::AGENT_MESSAGE,
            json!({"content": "Agent message"}).to_string(),
        ),
        (kinds::AGENT_MESSAGE, "Malformed envelope".to_owned()),
    ];
    for (kind, content) in cases {
        let original = signed(&p.alice, kind, &content, 10, &[&["h", CHANNEL]]);
        let envelope = kind == kinds::AGENT_MESSAGE;
        assert_eq!(
            fold(&p.relay, std::slice::from_ref(&original))[0].is_agent_envelope(),
            envelope
        );
        let original_id = original.id_hex();
        for replacement in [
            "Plain edit".to_owned(),
            json!({"content": "Envelope edit"}).to_string(),
        ] {
            let edit = signed(
                &p.alice,
                kinds::EDIT,
                &replacement,
                11,
                &[&["e", &original_id]],
            );
            let row = &fold(&p.relay, &[original.clone(), edit])[0];
            assert!(row.edited);
            assert_eq!(row.is_agent_envelope(), envelope);
            let expected = if envelope && replacement.starts_with('{') {
                "Envelope edit"
            } else {
                &replacement
            };
            assert_eq!(row.content, expected);
        }
    }
}

#[test]
fn projects_attachment_links_as_file_names() {
    let p = people();
    let url = format!("https://relay.test/media/{}.pdf", "a".repeat(64));
    let imeta = format!("url {url}");
    let event = message(
        &p.alice,
        CHANNEL,
        &format!("See [Aidys Cap - EU.bebe5b6f.pdf]({url}) now"),
        10,
        &[&["imeta", &imeta, "m application/pdf", "size 1536"]],
    );
    let rows = fold(&p.relay, &[event]);
    assert_eq!(rows[0].content, "See  now");
    assert_eq!(
        rows[0].attachments,
        [Attachment {
            size: Some(1536),
            ..file(&url, "application/pdf", Some("Aidys Cap - EU.bebe5b6f.pdf"))
        }]
    );
    assert_eq!(rows[0].attachment_seams, [4]);
}

#[test]
fn projects_attachment_link_references_as_file_names() {
    let p = people();
    let url = "https://relay.test/media/file.pdf";
    let imeta = format!("url {url}");
    let event = message(
        &p.alice,
        CHANNEL,
        &format!("Download [Quarterly Summary][file].\n\n[file]: {url}"),
        10,
        &[&["imeta", &imeta, "m application/pdf"]],
    );
    let rows = fold(&p.relay, &[event]);
    assert_eq!(rows[0].content, format!("Download .\n\n[file]: {url}"));
    assert_eq!(
        rows[0].attachments,
        [file(url, "application/pdf", Some("Quarterly Summary"))]
    );
}

/// One body, one imeta URL: the projected content and attachment name.
fn project_link(body: &str, url: &str) -> (String, Option<String>) {
    let p = people();
    let imeta = format!("url {url}");
    let event = message(
        &p.alice,
        CHANNEL,
        body,
        10,
        &[&["imeta", &imeta, "m application/pdf"]],
    );
    let row = fold(&p.relay, &[event]).remove(0);
    assert_eq!(row.attachments.len(), 1);
    (row.content.clone(), row.attachments[0].name.clone())
}

#[test]
fn strips_attachment_links_and_validates_their_labels() {
    let hash = "a".repeat(64);
    let hashed = format!("https://relay.test/media/{hash}.pdf");
    let report = "https://relay.test/media/report.pdf";
    let plain = "https://relay.test/media/file.pdf";
    let capped = "n".repeat(256);
    let cases = [
        // Autolinks strip; hash-shaped URL names are rejected.
        (format!("<{hashed}>"), hashed.as_str(), "", None),
        (format!("[]({report})"), report, "", Some("report.pdf")),
        (format!("<{report}>"), report, "", Some("report.pdf")),
        (format!("[{hash}.pdf]({hashed})"), hashed.as_str(), "", None),
        (
            format!("[report\u{202e}exe.pdf]({plain})"),
            plain,
            "",
            Some("file.pdf"),
        ),
        (
            format!("[report\nfinal.pdf]({plain})"),
            plain,
            "",
            Some("file.pdf"),
        ),
        (
            format!("[résumé-月報.pdf]({plain})"),
            plain,
            "",
            Some("résumé-月報.pdf"),
        ),
        (
            format!("[{}]({plain})", "n".repeat(300)),
            plain,
            "",
            Some(capped.as_str()),
        ),
        (
            format!("[First name]({plain}) and [Second name]({plain})"),
            plain,
            " and",
            Some("First name"),
        ),
    ];
    for (body, url, content, name) in cases {
        let (projected, projected_name) = project_link(&body, url);
        assert_eq!(projected, content, "{body}");
        assert_eq!(projected_name.as_deref(), name, "{body}");
    }
}

#[test]
fn leaves_non_attachment_links_untouched() {
    let p = people();
    let linked = "https://relay.test/docs/file.pdf";
    let event = message(
        &p.alice,
        CHANNEL,
        &format!("[Public copy]({linked})"),
        10,
        &[&[
            "imeta",
            "url https://relay.test/media/file.pdf",
            "m application/pdf",
        ]],
    );
    let rows = fold(&p.relay, &[event]);
    assert_eq!(rows[0].content, format!("[Public copy]({linked})"));
    assert_eq!(
        rows[0].attachments,
        [file(
            "https://relay.test/media/file.pdf",
            "application/pdf",
            Some("file.pdf")
        )]
    );
}

#[test]
fn strips_an_attachment_link_wrapping_an_image() {
    let p = people();
    let file_url = "https://relay.test/media/file.pdf";
    let image_url = "https://relay.test/i.png";
    let imeta = format!("url {file_url}");
    let event = message(
        &p.alice,
        CHANNEL,
        &format!("Please see [![thumb]({image_url})]({file_url}) before Friday."),
        10,
        &[&["imeta", &imeta, "m application/pdf"]],
    );
    let rows = fold(&p.relay, &[event]);
    assert_eq!(rows[0].content, "Please see  before Friday.");
    assert_eq!(
        rows[0].attachments,
        [
            file(file_url, "application/pdf", Some("file.pdf")),
            attachment(image_url, AttachmentKind::Image)
        ]
    );

    let link_url = "https://relay.test/docs/public";
    let event = message(
        &p.alice,
        CHANNEL,
        &format!("Please see [![thumb]({image_url})]({link_url}) before Friday."),
        10,
        &[],
    );
    let rows = fold(&p.relay, &[event]);
    assert_eq!(
        rows[0].content,
        format!("Please see []({link_url}) before Friday.")
    );
    assert_eq!(
        rows[0].attachments,
        [attachment(image_url, AttachmentKind::Image)]
    );
}

#[test]
fn unwraps_agent_envelopes_and_projects_images_through_one_url_policy() {
    let p = people();
    let body = "See ![shot](https://x.test/a.png \"title\"), ![clip](<https://x.test/b.mp4>), and ![reference][image].\n\n![blocked](https://user:secret@x.test/private.png)\n\n[image]: https://x.test/reference.jpg";
    let agent = signed(
        &p.bob,
        kinds::AGENT_MESSAGE,
        &json!({ "content": body }).to_string(),
        10,
        &[
            &["h", CHANNEL],
            &[
                "imeta",
                "url https://x.test/c.jpg",
                "m image/jpeg",
                "dim 1280x720",
                "image https://x.test/c-poster.jpg",
            ],
            &["imeta", "url http://insecure.test/d.jpg"],
            &["imeta", "url https://user:secret@x.test/e.jpg"],
        ],
    );
    let rows = fold(&p.relay, &[agent]);
    assert_eq!(
        rows[0].content,
        "See , , and .\n\n\n\n[image]: https://x.test/reference.jpg"
    );
    assert_eq!(
        rows[0].attachments,
        [
            Attachment {
                mime: Some("image/jpeg".into()),
                name: Some("c.jpg".into()),
                dimensions: Some(Dimensions {
                    width: 1280,
                    height: 720
                }),
                preview_url: Some("https://x.test/c-poster.jpg".into()),
                ..attachment("https://x.test/c.jpg", AttachmentKind::Image)
            },
            Attachment {
                name: Some("d.jpg".into()),
                insecure: true,
                ..attachment("http://insecure.test/d.jpg", AttachmentKind::File)
            },
            attachment("https://x.test/a.png", AttachmentKind::Image),
            attachment("https://x.test/b.mp4", AttachmentKind::Video),
            attachment("https://x.test/reference.jpg", AttachmentKind::Image),
        ]
    );
}

/// The reference drops plain-HTTP media; here it stays visible as an
/// unavailable file, and its body link gives way to the card.
#[test]
fn keeps_plain_http_media_as_unavailable_files() {
    let p = people();
    let hash = "a".repeat(64);
    let url = format!("http://127.0.0.1:53000/media/{hash}.png");
    let event = message(
        &p.alice,
        CHANNEL,
        &format!("Look\n\n![shot.png](<{url}>)\n\n![raw](http://x.test/raw.gif)"),
        10,
        &[&[
            "imeta",
            &format!("url {url}"),
            "m image/png",
            "size 2048",
            "filename shot.png",
        ]],
    );
    let rows = fold(&p.relay, &[event]);
    assert_eq!(rows[0].content.trim(), "Look");
    assert_eq!(
        rows[0].attachments,
        [
            Attachment {
                mime: Some("image/png".into()),
                size: Some(2048),
                name: Some("shot.png".into()),
                insecure: true,
                ..attachment(&url, AttachmentKind::File)
            },
            Attachment {
                name: Some("raw.gif".into()),
                insecure: true,
                ..attachment("http://x.test/raw.gif", AttachmentKind::File)
            },
        ]
    );
}

#[test]
fn marks_replacement_edits_without_changing_recipients() {
    let p = people();
    let bob_hex = p.bob.public_key().to_hex();
    let relay_hex = p.relay.public_key().to_hex();
    let original = message(&p.alice, CHANNEL, "Hello @Mic", 10, &[&["p", &bob_hex]]);
    let original_id = original.id_hex();
    let replacement = signed(
        &p.alice,
        kinds::EDIT,
        "Hello @Mic",
        11,
        &[
            &["h", CHANNEL],
            &["e", &original_id],
            &["buzz:mention-snapshot", "1"],
            &["mention", &relay_hex, "Mic"],
        ],
    );
    let edited = &fold(&p.relay, &[original.clone(), replacement])[0];
    assert!(edited.edited);
    assert_eq!(edited.mentions, [p.bob.public_key()]);
    assert!(!fold(&p.relay, &[original])[0].edited);
}

#[test]
fn classifies_generic_imeta_files_and_validates_metadata() {
    let keys = Keys::generate();
    let hash = format!("{}.pdf", "a".repeat(64));
    let hashed = format!("url https://x.test/relay/{hash}");
    let oversized = format!("size {}", "9".repeat(20));
    let event = message(
        &keys,
        "channel",
        "",
        1,
        &[
            &[
                "imeta",
                "url https://x.test/docs/report.pdf",
                "m application/pdf",
                "size 1536",
            ],
            &["imeta", "url https://x.test/audio.mp3", "m audio/mpeg"],
            &["imeta", &hashed, "m application/pdf", "size 0"],
            &[
                "imeta",
                "url https://x.test/broken.bin",
                "m application/octet-stream",
                "size 1.5",
            ],
            &[
                "imeta",
                "url https://x.test/oversized.dat",
                "m application/octet-stream",
                &oversized,
            ],
            &[
                "imeta",
                "url https://x.test/%zz",
                "m application/octet-stream",
            ],
        ],
    );
    assert_eq!(
        parse(&event, &[]),
        [
            Attachment {
                size: Some(1536),
                ..file(
                    "https://x.test/docs/report.pdf",
                    "application/pdf",
                    Some("report.pdf")
                )
            },
            Attachment {
                kind: AttachmentKind::Audio,
                ..file("https://x.test/audio.mp3", "audio/mpeg", Some("audio.mp3"))
            },
            file(
                &format!("https://x.test/relay/{hash}"),
                "application/pdf",
                None
            ),
            file(
                "https://x.test/broken.bin",
                "application/octet-stream",
                Some("broken.bin")
            ),
            file(
                "https://x.test/oversized.dat",
                "application/octet-stream",
                Some("oversized.dat")
            ),
            file(
                "https://x.test/%zz",
                "application/octet-stream",
                Some("%zz")
            ),
        ]
    );
}

#[test]
fn classifies_media_case_insensitively_and_by_legacy_extension() {
    let keys = Keys::generate();
    let event = message(
        &keys,
        "channel",
        "",
        1,
        &[
            &["imeta", "url https://x.test/photo", "m Image/PNG"],
            &["imeta", "url https://x.test/movie", "m Video/MP4"],
            &["imeta", "url https://x.test/photo.avif"],
            &["imeta", "url https://x.test/movie.webm"],
            &["imeta", "url https://x.test/archive"],
        ],
    );
    let kinds: Vec<(AttachmentKind, Option<String>)> = parse(&event, &[])
        .into_iter()
        .map(|a| (a.kind, a.name))
        .collect();
    let named = |kind, name: &str| (kind, Some(name.to_owned()));
    assert_eq!(
        kinds,
        [
            named(AttachmentKind::Image, "photo"),
            named(AttachmentKind::Video, "movie"),
            named(AttachmentKind::Image, "photo.avif"),
            named(AttachmentKind::Video, "movie.webm"),
            named(AttachmentKind::File, "archive"),
        ]
    );
}

#[test]
fn validates_url_derived_names() {
    let keys = Keys::generate();
    let event = message(
        &keys,
        "channel",
        "",
        1,
        &[
            &[
                "imeta",
                "url https://x.test/report%E2%80%AEexe.pdf",
                "m application/pdf",
            ],
            &[
                "imeta",
                "url https://x.test/r%C3%A9sum%C3%A9-%E6%9C%88%E5%A0%B1.pdf",
                "m application/pdf",
            ],
            &[
                "imeta",
                "url https://x.test/hash",
                "m application/pdf",
                "filename report\nfinal.pdf",
            ],
        ],
    );
    let names: Vec<Option<String>> = parse(&event, &[]).into_iter().map(|a| a.name).collect();
    assert_eq!(
        names,
        [None, Some("résumé-月報.pdf".into()), Some("hash".into())]
    );
}

#[test]
fn keeps_markdown_images_after_imeta_and_validates_layout_hints() {
    let keys = Keys::generate();
    let event = message(
        &keys,
        "channel",
        "",
        1,
        &[&[
            "imeta",
            "url https://x.test/original.png",
            "m image/png",
            "dim 700x900",
            "thumb https://x.test/thumbnail.png",
            "blurhash LEHV6nWB2yk8pyo0adR*.7kCMdnj",
        ]],
    );
    let parsed = parse(
        &event,
        &["https://x.test/original.png", "https://x.test/legacy.png"],
    );
    assert_eq!(
        parsed,
        [
            Attachment {
                mime: Some("image/png".into()),
                name: Some("original.png".into()),
                dimensions: Some(Dimensions {
                    width: 700,
                    height: 900
                }),
                blurhash: Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".into()),
                preview_url: Some("https://x.test/thumbnail.png".into()),
                ..attachment("https://x.test/original.png", AttachmentKind::Image)
            },
            attachment("https://x.test/legacy.png", AttachmentKind::Image),
        ]
    );
}

#[test]
fn classifies_voice_notes() {
    let keys = Keys::generate();
    let url = "https://x.test/media/hash";
    let imeta = format!("url {url}");
    let event = message(
        &keys,
        "channel",
        "",
        1,
        &[
            &[
                "imeta",
                &imeta,
                "m video/mp4",
                "filename voice-note-1.mp4",
                "duration 12.3",
                "waveform 0 50 100",
            ],
            &[
                "imeta",
                "url https://x.test/a",
                "m Video/MP4",
                "filename Voice-Note-1.MP4",
            ],
            &[
                "imeta",
                "url https://x.test/b",
                "m video/mp4; codecs=\"avc1.42E01E,mp4a.40.2\"",
                "filename voice-note-parameterized.mp4",
            ],
            &[
                "imeta",
                "url https://x.test/voice-note-fallback.mp4",
                "m video/mp4V-ES",
                "filename voice-note-1.mp4",
            ],
        ],
    );
    let parsed = parse(&event, &[]);
    assert_eq!(
        parsed[0],
        Attachment {
            kind: AttachmentKind::Audio,
            mime: Some("video/mp4".into()),
            name: Some("voice-note-1.mp4".into()),
            voice_note: true,
            duration: Some(12.3),
            waveform: Some(vec![0, 50, 100]),
            ..attachment(url, AttachmentKind::Audio)
        }
    );
    assert!(parsed[1].voice_note && parsed[1].kind == AttachmentKind::Audio);
    assert!(parsed[2].voice_note && parsed[2].kind == AttachmentKind::Audio);
    assert!(!parsed[3].voice_note && parsed[3].kind == AttachmentKind::Video);

    // A link label names a legacy voice note when `filename` is absent.
    let event = message(
        &keys,
        "channel",
        &format!("[voice-note-2.mp4]({url})"),
        1,
        &[&["imeta", &imeta, "m video/mp4"]],
    );
    let rows = fold_messages(
        "channel",
        &keys.public_key(),
        &[event],
        &FoldOptions::default(),
    );
    assert_eq!(rows[0].attachments[0].kind, AttachmentKind::Audio);
    assert!(rows[0].attachments[0].voice_note);
    assert_eq!(
        rows[0].attachments[0].name.as_deref(),
        Some("voice-note-2.mp4")
    );
}

#[test]
fn names_attachments_by_link_label_then_filename_then_basename() {
    let keys = Keys::generate();
    let event = message(
        &keys,
        "channel",
        "[Link Name](https://x.test/one.pdf)",
        1,
        &[
            &[
                "imeta",
                "url https://x.test/one.pdf",
                "m application/pdf",
                "filename File Name.pdf",
            ],
            &[
                "imeta",
                "url https://x.test/two.pdf",
                "m application/pdf",
                "filename File Name.pdf",
            ],
            &["imeta", "url https://x.test/three.pdf", "m application/pdf"],
        ],
    );
    let rows = fold_messages(
        "channel",
        &keys.public_key(),
        &[event],
        &FoldOptions::default(),
    );
    let names: Vec<Option<&str>> = rows[0]
        .attachments
        .iter()
        .map(|a| a.name.as_deref())
        .collect();
    assert_eq!(
        names,
        [Some("Link Name"), Some("File Name.pdf"), Some("three.pdf")]
    );
}

#[test]
fn sources_attachments_from_the_newest_edit_with_imeta() {
    let p = people();
    let url = "https://relay.test/media/report.pdf";
    let imeta = format!("url {url}");
    let original = message(
        &p.alice,
        CHANNEL,
        &format!("Caption [Report]({url})"),
        10,
        &[&["imeta", &imeta, "m application/pdf"]],
    );
    let original_id = original.id_hex();
    let edit = |content: String, at| {
        signed(
            &p.alice,
            kinds::EDIT,
            &content,
            at,
            &[&["h", CHANNEL], &["e", &original_id]],
        )
    };
    let first = edit(format!("Caption [Report]({url} \"Old title\")"), 11);
    let second = edit(format!("Caption [Report]({url} \"New title\")"), 12);
    let before = fold(&p.relay, &[original.clone(), first.clone()]);
    let next = fold(&p.relay, &[original.clone(), first.clone(), second.clone()]);
    assert_eq!(next[0].content, before[0].content);
    assert_eq!(next[0].attachments, before[0].attachments);
    assert_eq!(
        next[0].source_content.as_deref(),
        Some(second.content.as_str())
    );
    assert_ne!(*next[0], *before[0]);

    // Provenance changes even when the presentation does not.
    let image = "url https://relay.test/media/i.png";
    let captioned = message(
        &p.alice,
        CHANNEL,
        "Caption",
        10,
        &[&["imeta", image, "m image/png"]],
    );
    let captioned_id = captioned.id_hex();
    let media = |at| {
        signed(
            &p.alice,
            kinds::EDIT,
            "Caption",
            at,
            &[&["e", &captioned_id], &["imeta", image, "m image/png"]],
        )
    };
    let (one, two) = (media(11), media(12));
    let rows = fold(&p.relay, &[captioned, one, two.clone()]);
    assert_eq!(rows[0].attachment_source_id, Some(two.id));
}

#[test]
fn keeps_reference_identities_separate_from_recipients() {
    let p = people();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let c = "c".repeat(64);
    let key = |hex: &str| PublicKey::from_hex(hex).unwrap();
    type Case<'a> = (&'a [&'a [&'a str]], Vec<PublicKey>, Vec<PublicKey>);
    let cases: [Case; 6] = [
        (&[&["mention", &b]], vec![], vec![key(&b)]),
        (&[&["p", &a]], vec![key(&a)], vec![]),
        (
            &[&["mention", &c], &["mention", &b], &["mention", &c]],
            vec![],
            vec![key(&c), key(&b)],
        ),
        (&[&["mention", &b, "agent-address"]], vec![], vec![]),
        (
            &[
                &["mention", "invalid"],
                &["mention", &b.to_uppercase()],
                &["p", "invalid"],
            ],
            vec![],
            vec![],
        ),
        (
            &[&["p", &a], &["mention", &a]],
            vec![key(&a)],
            vec![key(&a)],
        ),
    ];
    for (tags, mentions, references) in cases {
        let row = fold(
            &p.relay,
            &[message(&p.alice, CHANNEL, "@Someone", 10, tags)],
        )
        .remove(0);
        assert_eq!(row.mentions, mentions);
        assert_eq!(row.mention_references, references);
    }
}

#[test]
fn reads_sent_from_thread_and_emoji_tags() {
    let p = people();
    let root = "f".repeat(64);
    let long = "é".repeat(80);
    let event = message(
        &p.alice,
        CHANNEL,
        ":Party: hi",
        10,
        &[
            &["buzz:sent-from-thread", "not-hex", "ignored"],
            &["buzz:sent-from-thread", &root, &long],
            &["emoji", ":Party:", " https://x.test/p.png "],
            &["emoji", "party", "https://x.test/second.png"],
            &["emoji", "bad code", "https://x.test/bad.png"],
        ],
    );
    let row = fold(&p.relay, &[event]).remove(0);
    let sent = row.sent_from_thread.clone().unwrap();
    assert_eq!(sent.root_id, EventId::from_hex(&root).unwrap());
    assert_eq!(sent.excerpt.unwrap().chars().count(), 64);
    assert_eq!(
        row.emoji,
        [CustomEmoji {
            shortcode: "party".into(),
            url: "https://x.test/p.png".into()
        }]
    );
}

#[test]
fn folds_relay_signed_membership_rows_only() {
    let p = people();
    let (alice, bob) = (p.alice.public_key().to_hex(), p.bob.public_key().to_hex());
    let row = |keys: &Keys, body: serde_json::Value| {
        signed(
            keys,
            kinds::SYSTEM_MESSAGE,
            &body.to_string(),
            10,
            &[&["h", CHANNEL]],
        )
    };
    let joined = row(
        &p.relay,
        json!({"type": "member_joined", "actor": alice, "target": bob}),
    );
    let left = row(&p.relay, json!({"type": "member_left", "actor": bob}));
    let rows = fold(
        &p.relay,
        &[
            joined.clone(),
            left.clone(),
            row(&p.alice, json!({"type": "member_left", "actor": alice})),
            row(
                &p.relay,
                json!({"type": "member_left", "actor": bob, "target": alice}),
            ),
            row(&p.relay, json!({"type": "topic_changed", "actor": alice})),
            row(&p.relay, json!({"type": "member_removed", "actor": alice})),
        ],
    );
    let mut expected = [joined.id, left.id];
    expected.sort();
    assert_eq!(ids(&rows), expected);
    let changes: HashSet<MembershipChange> = rows.iter().filter_map(|r| r.membership).collect();
    assert_eq!(
        changes,
        HashSet::from([
            MembershipChange {
                kind: MembershipKind::Joined,
                actor: p.alice.public_key(),
                target: p.bob.public_key(),
            },
            MembershipChange {
                kind: MembershipKind::Left,
                actor: p.bob.public_key(),
                target: p.bob.public_key(),
            },
        ])
    );
    assert!(rows.iter().all(|r| r.content.is_empty()));
}

#[test]
fn resolves_marked_ancestry_never_bare_references_or_a_lone_root() {
    let p = people();
    let root = message(&p.alice, CHANNEL, "root", 1, &[]);
    let root_id = root.id_hex();
    let reference =
        |tags: &[&[&str]]| thread_reference(&message(&p.alice, CHANNEL, "test", 1, tags));
    assert_eq!(reference(&[&["e", &root_id]]), None);
    assert_eq!(reference(&[&["e", &root_id, "", "root"]]), None);
    let direct = Some(ThreadReference {
        root_id: root.id,
        parent_id: root.id,
    });
    assert_eq!(reference(&[&["e", &root_id, "", "reply"]]), direct);
    assert_eq!(
        reference(&[&["e", &root_id, "", "reply"], &["e", "bad", "", "reply"]]),
        direct
    );
    let parent = "a".repeat(64);
    let nested = Some(ThreadReference {
        root_id: root.id,
        parent_id: EventId::from_hex(&parent).unwrap(),
    });
    assert_eq!(
        reference(&[
            &["e", &root_id.to_uppercase(), "", "root"],
            &["e", &parent, "", "reply"]
        ]),
        nested
    );
}

#[test]
fn groups_reactions_by_event_local_emoji_identity() {
    let (viewer, other, relay) = (Keys::generate(), Keys::generate(), Keys::generate());
    let root = message(&other, "c", "React here", 1, &[]);
    let root_id = root.id_hex();
    let reaction = |keys: &Keys, at| {
        signed(
            keys,
            kinds::REACTION,
            "👍",
            at,
            &[&["e", &root_id], &["h", "c"]],
        )
    };
    let custom = |url: &str, at| {
        signed(
            &viewer,
            kinds::REACTION,
            ":party:",
            at,
            &[&["e", &root_id], &["emoji", "party", url]],
        )
    };
    let (mine, duplicate, theirs) = (
        reaction(&viewer, 2),
        reaction(&viewer, 3),
        reaction(&other, 2),
    );
    let fold_c = |events: &[Verified]| {
        fold_messages("c", &relay.public_key(), events, &FoldOptions::default())
    };
    let rows = fold_c(&[
        root.clone(),
        mine.clone(),
        duplicate.clone(),
        theirs.clone(),
        custom("https://one.test/a", 5),
        custom("https://two.test/a", 6),
    ]);
    assert_eq!(rows[0].reactions.len(), 3);
    assert_eq!(
        rows[0].reactions[0].events,
        [
            Reaction {
                id: mine.id,
                author: viewer.public_key()
            },
            Reaction {
                id: duplicate.id,
                author: viewer.public_key()
            },
            Reaction {
                id: theirs.id,
                author: other.public_key()
            },
        ]
    );
    assert_eq!(
        rows[0].reactions[1].emoji.as_ref().unwrap().url,
        "https://one.test/a"
    );

    // Pill order follows the first reaction, even after it is removed.
    let pill = |keys: &Keys, content: &str, at| {
        signed(
            keys,
            kinds::REACTION,
            content,
            at,
            &[&["e", &root_id], &["h", "c"]],
        )
    };
    let heart = pill(&viewer, "❤️", 2);
    let events = vec![
        root.clone(),
        pill(&other, "😂", 3),
        pill(&other, "❤️", 4),
        heart.clone(),
    ];
    let order = |events: &[Verified]| -> Vec<String> {
        fold_c(events)[0]
            .reactions
            .iter()
            .map(|g| g.content.clone())
            .collect()
    };
    assert_eq!(order(&events), ["❤️", "😂"]);
    let heart_id = heart.id_hex();
    let mut removed = events.clone();
    removed.push(signed(
        &viewer,
        kinds::DELETION,
        "",
        5,
        &[&["e", &heart_id], &["h", "c"], &["k", "7"]],
    ));
    assert_eq!(order(&removed), ["❤️", "😂"]);
}

#[test]
fn orders_by_valid_ms_tags_then_id() {
    let p = people();
    let tagged =
        |content: &str, second, ms: &str| message(&p.alice, "c", content, second, &[&["ms", ms]]);
    let late = tagged("late", 100, "900");
    let early = tagged("early", 100, "100");
    let untagged = message(&p.alice, "c", "untagged", 100, &[]);
    assert_eq!(event_ms(&late), 100_900);
    assert_eq!(event_ms(&untagged), 100_000);
    for value in [
        "", "-1", "100900", "1.5", "1e2", "+1", " 1", "1 ", "01", "000", "NaN", "1000", "100.5e3",
    ] {
        assert_eq!(event_ms(&tagged("x", 100, value)), 100_000, "{value:?}");
    }
    for offset in [0, 7, 99, 999] {
        assert_eq!(
            event_ms(&tagged("x", 100, &offset.to_string())),
            100_000 + offset
        );
    }
    let fold_c = |events: &[Verified]| {
        ids(&fold_messages(
            "c",
            &p.relay.public_key(),
            events,
            &FoldOptions::default(),
        ))
    };
    assert_eq!(
        fold_c(&[late.clone(), untagged.clone(), early.clone()]),
        [untagged.id, early.id, late.id]
    );

    let (x, y) = (tagged("x", 100, "500"), tagged("y", 100, "500"));
    let (first, second) = if x.id.to_hex() < y.id.to_hex() {
        (x, y)
    } else {
        (y, x)
    };
    assert_eq!(
        fold_c(&[second.clone(), first.clone()]),
        [first.id, second.id]
    );

    let last = tagged("last", 100, "999");
    let next = tagged("next", 101, "0");
    assert_eq!(fold_c(&[next.clone(), last.clone()]), [last.id, next.id]);
}

// MessageProjection

fn projection(relay: &Keys, include_replies: bool) -> MessageProjection {
    MessageProjection::new(
        "c",
        relay.public_key(),
        FoldOptions {
            include_replies,
            ..Default::default()
        },
    )
}

#[test]
fn renders_channel_and_thread_orderings_identically() {
    let p = people();
    let root = message(&p.alice, "c", "root", 10, &[]);
    let root_id = root.id_hex();
    let reply = |content: &str, ms: Option<&str>| {
        let marker = ["e", root_id.as_str(), "", "reply"];
        let mut tags: Vec<&[&str]> = vec![&marker];
        let ms_tag;
        if let Some(ms) = ms {
            ms_tag = ["ms", ms];
            tags.push(&ms_tag);
        }
        message(&p.alice, "c", content, 20, &tags)
    };
    let mut events = vec![
        root,
        reply("c", Some("900")),
        reply("a", None),
        reply("b", Some("100")),
        reply("d", Some("100")),
        reply("e", Some("1000")),
    ];
    let (channel, _) = projection(&p.relay, true).reconcile(&events);
    events.reverse();
    let thread = fold_messages(
        "c",
        &p.relay.public_key(),
        &events,
        &FoldOptions {
            include_replies: true,
            ..Default::default()
        },
    );
    assert_eq!(ids(&channel), ids(&thread));
    assert_eq!(channel.len(), 6);
}

#[test]
fn applies_delete_of_reaction_incrementally_and_rejects_foreign_deletes() {
    let (viewer, other, relay) = (Keys::generate(), Keys::generate(), Keys::generate());
    let root = message(&other, "c", "React here", 1, &[]);
    let root_id = root.id_hex();
    let mine = signed(
        &viewer,
        kinds::REACTION,
        "👍",
        2,
        &[&["e", &root_id], &["h", "c"]],
    );
    let mine_id = mine.id_hex();
    let remove = |keys: &Keys| {
        signed(
            keys,
            kinds::DELETION,
            "",
            3,
            &[&["e", &mine_id], &["h", "c"], &["k", "7"]],
        )
    };
    let (deletion, foreign) = (remove(&viewer), remove(&other));
    let mut projection = projection(&relay, false);
    let reactions = |projection: &mut MessageProjection, events: &[Verified]| {
        projection.reconcile(events).0[0].reactions.len()
    };
    assert_eq!(reactions(&mut projection, &[root.clone(), mine.clone()]), 1);
    assert_eq!(
        reactions(&mut projection, &[root.clone(), mine.clone(), foreign]),
        1
    );
    assert_eq!(
        reactions(
            &mut projection,
            &[root.clone(), mine.clone(), deletion.clone()]
        ),
        0
    );
    // Removing the deletion (a failed outbox op) restores the reaction.
    assert_eq!(reactions(&mut projection, &[root.clone(), mine.clone()]), 1);
    assert_eq!(reactions(&mut projection, &[root, deletion, mine]), 0);
}

#[test]
fn incremental_edits_match_a_full_fold_and_roll_back_on_delete() {
    let p = people();
    let file_url = "https://relay.test/media/file.pdf";
    let file_imeta = format!("url {file_url}");
    let image_imeta = "url https://relay.test/media/image.png";
    for kind in [kinds::CHAT, kinds::AGENT_MESSAGE] {
        let original = signed(
            &p.alice,
            kind,
            "Original",
            10,
            &[
                &["h", CHANNEL],
                &["imeta", &file_imeta, "m application/pdf"],
            ],
        );
        let id = original.id_hex();
        let media = signed(
            &p.alice,
            kinds::EDIT,
            "Current",
            11,
            &[
                &["h", CHANNEL],
                &["e", &id],
                &["imeta", image_imeta, "m image/png"],
            ],
        );
        let text = signed(
            &p.alice,
            kinds::EDIT,
            "Latest caption",
            12,
            &[&["h", CHANNEL], &["e", &id]],
        );
        let spoof = signed(
            &Keys::generate(),
            kinds::EDIT,
            "Spoof",
            13,
            &[
                &["h", CHANNEL],
                &["e", &id],
                &["imeta", &file_imeta, "m application/pdf"],
            ],
        );
        let mut projection =
            MessageProjection::new(CHANNEL, p.relay.public_key(), FoldOptions::default());
        let (before, _) = projection.reconcile(&[original.clone(), media.clone()]);
        let all = [original.clone(), media.clone(), text, spoof];
        let (after, changed) = projection.reconcile(&all);
        assert!(changed);
        assert_eq!(after[0].content, "Latest caption");
        assert_eq!(after[0].attachment_source_id, Some(media.id));
        assert_eq!(after[0].attachments, before[0].attachments);
        assert_eq!(*after, *fold(&p.relay, &all));

        let media_id = media.id_hex();
        let deleted = signed(
            &p.alice,
            kinds::DELETION,
            "",
            14,
            &[&["h", CHANNEL], &["e", &media_id]],
        );
        let mut restored_events = all.to_vec();
        restored_events.push(deleted);
        let (restored, _) = projection.reconcile(&restored_events);
        assert_eq!(restored[0].attachment_source_id, None);
        let urls: Vec<&str> = restored[0]
            .attachments
            .iter()
            .map(|a| a.url.as_str())
            .collect();
        assert_eq!(urls, [file_url]);
    }
}

#[test]
fn keeps_row_and_snapshot_identity_when_nothing_changed() {
    let p = people();
    let a = message(&p.alice, "c", "a", 10, &[]);
    let b = message(&p.bob, "c", "b", 11, &[]);
    let mut projection = projection(&p.relay, false);
    let (first, changed) = projection.reconcile(&[a.clone(), b.clone()]);
    assert!(changed);

    let (same, changed) = projection.reconcile(&[b.clone(), a.clone()]);
    assert!(!changed);
    assert!(Arc::ptr_eq(&first, &same));

    // A reaction on `a` replaces only `a`'s row.
    let a_id = a.id_hex();
    let like = signed(&p.bob, kinds::REACTION, "👍", 12, &[&["e", &a_id]]);
    let (liked, changed) = projection.reconcile(&[a.clone(), b.clone(), like.clone()]);
    assert!(changed);
    assert!(!Arc::ptr_eq(&first[0], &liked[0]));
    assert!(Arc::ptr_eq(&first[1], &liked[1]));

    // A refold that produces an equal row keeps its `Arc` (a foreign edit).
    let foreign = signed(&p.bob, kinds::EDIT, "not yours", 13, &[&["e", &a_id]]);
    let (edited, changed) = projection.reconcile(&[a, b, like, foreign]);
    assert!(!changed);
    assert!(Arc::ptr_eq(&liked, &edited));
}

#[test]
fn patches_delivery_without_refolding_and_orders_new_rows() {
    let p = people();
    let mut projection = projection(&p.relay, false);
    let later = message(&p.alice, "c", "later", 20, &[]);
    let earlier = message(&p.bob, "c", "earlier", 10, &[]);
    let sending = LocalDelivery {
        status: Delivery::Sending,
        error: None,
    };
    let (rows, _) = projection.reconcile_local([&later], [(later.id, sending.clone())]);
    assert_eq!(rows[0].delivery, Some(sending));

    let failed = LocalDelivery {
        status: Delivery::Failed,
        error: Some("Nope".into()),
    };
    let (rows, changed) = projection.reconcile_local([&later], [(later.id, failed.clone())]);
    assert!(changed);
    assert_eq!(rows[0].delivery, Some(failed));

    let (rows, changed) = projection.reconcile([&later, &earlier]);
    assert!(changed);
    assert_eq!(ids(&rows), [earlier.id, later.id]);
    assert_eq!(rows[1].delivery, None);

    let (rows, changed) = projection.reconcile([&later]);
    assert!(changed);
    assert_eq!(ids(&rows), [later.id]);
}

#[test]
fn raises_the_shared_clock_and_refolds_on_new_options() {
    let p = people();
    let clock = Arc::new(MessageClock::new());
    let mut projection = projection(&p.relay, false).with_clock(clock.clone());
    let target = message(&p.alice, "c", "spam", 1_000, &[&["ms", "250"]]);
    let target_id = target.id_hex();
    let moderation = signed(
        &p.bob,
        kinds::MODERATOR_DELETE,
        "",
        1_001,
        &[&["h", "c"], &["e", &target_id]],
    );
    let (rows, _) = projection.reconcile(&[target, moderation]);
    assert_eq!(rows.len(), 1);
    assert_eq!(clock.next("c", 1_000_000), 1_000_251);

    let (rows, changed) = projection.set_options(FoldOptions {
        channel_admins: HashSet::from([p.bob.public_key()]),
        ..Default::default()
    });
    assert!(changed);
    assert!(rows.is_empty());
}
