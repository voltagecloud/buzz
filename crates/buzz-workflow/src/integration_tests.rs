//! Integration workflows use the production trigger extraction and filter path.

use super::*;
use nostr::{EventBuilder, Keys, Kind, Tag};

#[tokio::test]
async fn trusted_integration_messages_drive_filtered_thread_workflows() {
    let publisher = Keys::generate();
    let other = Keys::generate();
    let yaml = format!(
        r#"
name: Route integration work
trigger:
  on: integration_event
  source: linear
  action: issue.update
  authors: ["{}"]
  filter: 'str_contains(trigger_labels, "agent")'
steps:
  - id: delegate
    action: send_message
    reply_in_thread: true
    text: "Review {{{{trigger.url}}}}"
"#,
        publisher.public_key().to_hex()
    );
    let (def, _) = schema::parse_yaml(&yaml).unwrap();
    let make = |keys: &Keys, source: &str, action: &str, labels: &str| {
        let event = EventBuilder::new(Kind::Custom(9), "Issue changed")
            .tags([
                Tag::parse(["ext-source", source]).unwrap(),
                Tag::parse(["ext-action", action]).unwrap(),
                Tag::parse(["ext-labels", labels]).unwrap(),
                Tag::parse(["ext-url", "https://linear.app/team/issue/ABC-1"]).unwrap(),
                Tag::parse(["ext-author", "spoof"]).unwrap(),
            ])
            .sign_with_keys(keys)
            .unwrap();
        buzz_core::StoredEvent::new(event, Some(Uuid::new_v4()))
    };
    for (keys, source, action, labels, expected) in [
        (&publisher, "linear", "issue.update", "agent,bug", true),
        (&other, "linear", "issue.update", "agent", false),
        (&publisher, "github", "issue.update", "agent", false),
        (&publisher, "linear", "issue.create", "agent", false),
        (&publisher, "linear", "", "agent", false),
        (&publisher, "", "issue.update", "agent", false),
        (&publisher, "linear", "issue.update", "bug", false),
    ] {
        let stored = make(keys, source, action, labels);
        let ctx = build_trigger_context(&stored);
        assert_eq!(ctx.author, keys.public_key().to_hex());
        assert_eq!(ctx.message_id, stored.event.id.to_hex());
        assert!(trigger_matches_event(&def.trigger, 9));
        assert!(!trigger_matches_event(
            &def.trigger,
            buzz_core::kind::KIND_INTEGRATION_RECORD
        ));
        assert_eq!(
            should_fire_workflow(&def, &ctx, Uuid::new_v4()).await,
            expected,
            "{source}/{action} by {} with {labels}",
            ctx.author
        );
        assert_eq!(
            executor::resolve_template("{{trigger.author}} {{trigger.url}}", &ctx, &HashMap::new())
                .unwrap(),
            format!(
                "{} https://linear.app/team/issue/ABC-1",
                keys.public_key().to_hex()
            )
        );
    }
    // A source-wide workflow still requires a nonempty action tag. Uppercase
    // configured hex keys identify the same trusted publisher.
    let mut any_action = def.clone();
    if let TriggerDef::IntegrationEvent {
        action, authors, ..
    } = &mut any_action.trigger
    {
        *action = None;
        authors[0] = authors[0].to_uppercase();
    }
    for (action, expected) in [("", false), ("issue.create", true)] {
        let stored = make(&publisher, "linear", action, "agent");
        assert_eq!(
            should_fire_workflow(&any_action, &build_trigger_context(&stored), Uuid::new_v4())
                .await,
            expected
        );
    }

    // Missing/untrusted publishers cannot be admitted through YAML validation.
    for replacement in ["[]", "[bad-key]"] {
        let invalid = yaml.replace(
            &format!("[\"{}\"]", publisher.public_key().to_hex()),
            replacement,
        );
        assert!(schema::parse_yaml(&invalid).is_err());
    }
}
