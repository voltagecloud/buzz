//! Workflow configuration against the local test relay: create, list,
//! update, run, read runs and delete, in a throwaway channel. Skipped
//! unless `BUZZ_TEST_RELAY` points at `.test-relay`.

use buzz_identity::Identity;
use buzz_relay_client::lifecycle::NewChannel;
use buzz_relay_client::workflows::{
    Action, Landing, Outcome, ViewStatus, WorkflowDefinition, WorkflowOperation, Workflows,
};
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

struct Fixture {
    origin: RelayOrigin,
    identities: Value,
}

fn fixture() -> Option<Fixture> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    Some(Fixture { origin, identities })
}

impl Fixture {
    async fn session(&self, name: &str) -> Session {
        let nsec = self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap();
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            Arc::new(Identity::from_nsec(nsec).unwrap()),
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the roster and live updates", |s| {
            s.roster == RosterStatus::Ready && s.live
        })
        .await;
        session
    }
}

async fn until<T: Clone>(
    receiver: &mut watch::Receiver<T>,
    what: &str,
    predicate: impl Fn(&T) -> bool,
) -> T {
    tokio::time::timeout(
        Duration::from_secs(20),
        receiver.wait_for(|value| predicate(value)),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    .unwrap()
    .clone()
}

/// The operation for `id` once it left `pending`.
async fn settled(workflows: &Workflows, id: &str) -> WorkflowOperation {
    let mut operations = workflows.watch_operations();
    let all = until(&mut operations, "the command's outcome", |ops| {
        ops.iter()
            .any(|op| op.event_id == id && op.outcome != Outcome::Pending)
    })
    .await;
    all.iter().find(|op| op.event_id == id).unwrap().clone()
}

async fn read(workflows: &Workflows, channel: &str) -> Vec<WorkflowDefinition> {
    let view = workflows.definitions(vec![channel.into()], false).unwrap();
    view.refresh(false).await;
    let snapshot = view.snapshot();
    assert_eq!(snapshot.status, ViewStatus::Ready, "{:?}", snapshot.error);
    snapshot.data.items.clone()
}

const YAML: &str = "# kept\nname: Relay test\nenabled: false\ntrigger:\n  on: message_posted\n  filter: str_contains(trigger_text, \"never-matches-anything\")\nsteps:\n  - id: say\n    action: send_message\n    text: hello\n";

#[tokio::test(flavor = "multi_thread")]
async fn saves_lists_runs_and_deletes_a_workflow() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let tag = std::process::id();
    let alice = fixture.session("alice").await;
    let channel = alice
        .create_channel(NewChannel {
            name: format!("#workflows-{tag}"),
            description: "Created by a test".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    let workflows = alice.workflows().clone();
    // The capability learns the new channel from the roster.
    let mut snapshot = alice.snapshot();
    until(&mut snapshot, "the new channel", |s| {
        s.channels.iter().any(|c| c.id == channel)
    })
    .await;
    assert!(read(&workflows, &channel).await.is_empty());

    // Create.
    let save = workflows.save(&channel, YAML.into(), None).unwrap();
    let operation = settled(&workflows, &save).await;
    assert_eq!(operation.action, Action::Save);
    assert_eq!(
        operation.outcome,
        Outcome::Succeeded,
        "{:?}",
        operation.error
    );
    let rows = read(&workflows, &channel).await;
    assert_eq!(rows.len(), 1);
    let saved = rows[0].clone();
    assert_eq!(saved.revision, save, "exact readback of the signed save");
    assert_eq!(saved.yaml, YAML, "the original text is kept");
    assert_eq!(saved.owner, alice.viewer());

    // The landing batch reads it too.
    let landing = Landing::new(workflows.clone(), tokio::runtime::Handle::current());
    landing.channels(vec![channel.clone()]);
    let mut shown = landing.watch();
    let landed = until(&mut shown, "the landing read", |s| {
        s.snapshots
            .get(&channel)
            .is_some_and(|c| c.status == ViewStatus::Ready)
    })
    .await;
    assert_eq!(landed.snapshots[&channel].data.items.len(), 1);
    drop(landing);

    // Update with the signed expected revision, a second later: the relay
    // treats a same-second replacement as superseded.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let edited = YAML.replace("Relay test", "Relay test edited");
    let update = workflows
        .save(&channel, edited.clone(), Some(&saved))
        .unwrap();
    let operation = settled(&workflows, &update).await;
    assert_eq!(
        operation.outcome,
        Outcome::Succeeded,
        "{:?}",
        operation.error
    );
    let rows = read(&workflows, &channel).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].yaml, edited);
    let current = rows[0].clone();
    // A stale revision is refused by the relay.
    let stale = workflows.save(&channel, YAML.into(), Some(&saved)).unwrap();
    let operation = settled(&workflows, &stale).await;
    assert_ne!(operation.outcome, Outcome::Succeeded);

    // Run now: the relay returns a run ID.
    let run = workflows.trigger(&current).unwrap();
    let operation = settled(&workflows, &run).await;
    assert_eq!(operation.action, Action::Trigger);
    if operation.outcome == Outcome::Succeeded {
        let run_id = operation.run_id.clone().expect("a run ID");
        // Run history over the authenticated route.
        let mut found = false;
        for _ in 0..20 {
            let runs = workflows.runs(&current, None).unwrap();
            runs.refresh(false).await;
            let page = runs.snapshot();
            assert_eq!(page.status, ViewStatus::Ready, "{:?}", page.error);
            if page.data.runs.iter().any(|r| r.id == run_id) {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(found, "the requested run is in its history");
    } else {
        eprintln!("relay did not run the workflow: {:?}", operation.error);
    }

    // Delete, then a fresh read no longer returns it.
    let delete = workflows.delete(&current).unwrap();
    let operation = settled(&workflows, &delete).await;
    assert_eq!(operation.action, Action::Delete);
    assert_ne!(
        operation.outcome,
        Outcome::Rejected,
        "{:?}",
        operation.error
    );
    let rows = read(&workflows, &channel).await;
    assert!(rows.is_empty(), "deleted: {rows:?}");

    // Dismissal clears the notices.
    for id in [save, update, stale, run, delete] {
        workflows.dismiss(&id).await.unwrap();
    }
    assert!(workflows.operations().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_commands_outside_participating_channels() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let bob = fixture.session("bob").await;
    let workflows = bob.workflows().clone();
    let elsewhere = "00000000-0000-4000-8000-000000000000";
    assert_eq!(
        workflows.save(elsewhere, YAML.into(), None),
        Err("Workflow access unavailable; refresh channel membership".into())
    );
    let view = workflows
        .definitions(vec![elsewhere.into()], false)
        .unwrap();
    assert_eq!(view.snapshot().status, ViewStatus::Unavailable);
    // A webhook URL is only for display.
    let hook = workflows
        .webhook_url("55555555-5555-4555-8555-555555555555")
        .unwrap();
    assert!(hook.ends_with("/hooks/55555555-5555-4555-8555-555555555555"));
}
