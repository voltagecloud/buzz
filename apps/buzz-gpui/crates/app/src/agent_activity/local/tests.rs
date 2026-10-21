use super::*;

const SCOPE: &str = "https://relay.test:viewer";

fn begin(store: &LocalActivity, message: &str) -> WorkHandle {
    store.begin(
        Begin {
            scope: SCOPE.into(),
            agent: "a".repeat(64),
            channel_id: "channel".into(),
            message_id: message.into(),
            thread_root_id: "b".repeat(64),
        },
        1,
    )
}

#[test]
fn keeps_storage_bounded_and_ignores_evicted_or_cleared_handles() {
    let store = LocalActivity::default();
    let old = begin(&store, "old");
    for n in 0..520 {
        begin(&store, &n.to_string());
    }
    assert_eq!(store.work().len(), 512);
    old.finish(WorkOutcome::failed("Late error"));
    assert!(!store.work().iter().any(|row| row.message_id == "old"));
    let pending = begin(&store, "reply");
    store.clear(Some(SCOPE), Some(&"a".repeat(64)));
    pending.started();
    pending.finish(WorkOutcome::ok());
    assert!(store.work().is_empty());
}

#[test]
fn shows_the_latest_step_while_working_and_drops_it_once_settled() {
    let store = LocalActivity::default();
    let work = begin(&store, "m");
    let row = || store.work()[0].clone();
    work.step("Read", Some("  notes.txt "));
    assert_eq!(row().state, LocalWorkState::Working);
    assert_eq!(
        row().step,
        Some(WorkStep {
            tool: "Read".into(),
            target: Some("notes.txt".into())
        })
    );
    let label =
        |row: LocalWork| local_work_label(row.state, row.error.as_deref(), row.step.as_ref());
    assert_eq!(label(row()), "Reading notes.txt");
    work.step("   ", None);
    assert_eq!(row().step.map(|step| step.tool).as_deref(), Some("Read"));
    work.finish(WorkOutcome::ok());
    assert_eq!(row().step, None);
    work.step("Late", None);
    assert_eq!(row().state, LocalWorkState::Done);
    assert_eq!(label(row()), "Finished");
}

#[test]
fn a_newer_handling_replaces_the_older_one() {
    let store = LocalActivity::default();
    let first = begin(&store, "m");
    first.finish(WorkOutcome::failed("Provider unavailable"));
    let next = begin(&store, "m");
    first.finish(WorkOutcome::ok());
    assert_eq!(store.work().len(), 1);
    assert_eq!(store.work()[0].state, LocalWorkState::Waiting);
    next.started();
    next.finish(WorkOutcome::failed(""));
    assert_eq!(
        store.work()[0].error.as_deref(),
        Some("Could not finish this request")
    );
    store.dismiss_error(SCOPE, &"a".repeat(64), "m");
    assert!(store.work()[0].dismissed);
}

#[test]
fn words_readable_steps_from_the_harness_names() {
    let text = |tool: &str, target: Option<&str>| {
        step_text(&WorkStep {
            tool: tool.into(),
            target: target.map(str::to_string),
        })
    };
    assert_eq!(
        text("Bash", Some("Custom harness catalog build")),
        "Custom harness catalog build"
    );
    assert_eq!(text("Shell", Some("ls")), "Running ls");
    assert_eq!(text("Shell", None), "Running a command");
    assert_eq!(text("Edit", Some("discovery.rs")), "Editing discovery.rs");
    assert_eq!(
        text("Agent", Some("Find harness")),
        "Subagent · Find harness"
    );
    assert_eq!(text("send", None), "Sending a message");
    assert_eq!(text("Bash", None), "Running a command");
    assert_eq!(text("custom_tool", Some("x")), "custom_tool · x");
    assert_eq!(text("custom_tool", None), "custom_tool");
}

#[test]
fn ranks_working_over_waiting_over_failed() {
    let store = LocalActivity::default();
    let failed = begin(&store, "one");
    failed.finish(WorkOutcome::failed("x"));
    let waiting = begin(&store, "two");
    let work = store.work();
    let agents = local_activity(&work, "channel", None);
    assert_eq!(agents.values().next().unwrap().message_id, "two");
    assert!(breathing(agents.values().next().copied()));
    waiting.step("Read", Some("a.rs"));
    let work = store.work();
    assert!(!breathing(
        local_activity(&work, "channel", None)
            .values()
            .next()
            .copied()
    ));
}
