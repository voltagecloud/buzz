use super::*;
use crate::agent_activity::local::{Begin, LocalActivity, LocalWorkState};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn counted() -> (Arc<AtomicUsize>, End) {
    let ended = Arc::new(AtomicUsize::new(0));
    let end = {
        let ended = ended.clone();
        Box::new(move || {
            ended.fetch_add(1, Ordering::SeqCst);
        }) as End
    };
    (ended, end)
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn ends_once_the_run_and_everything_it_handed_off_have_settled() {
    let (ended, end) = counted();
    let working = Working::new(vec![end], None, tokio::runtime::Handle::current());
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    working.until(async move {
        let _ = finished.await;
        WorkOutcome::ok()
    });
    // `run` returned, but the turn it handed off is still going.
    working.release();
    settle().await;
    assert_eq!(ended.load(Ordering::SeqCst), 0);
    done.send(()).unwrap();
    settle().await;
    assert_eq!(ended.load(Ordering::SeqCst), 1);
    // Once over, nothing holds it again or ends it twice.
    working.until(async { WorkOutcome::ok() });
    working.release();
    settle().await;
    assert_eq!(ended.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_run_that_hands_nothing_off_ends_when_it_returns() {
    let (ended, end) = counted();
    let working = Working::new(vec![end], None, tokio::runtime::Handle::current());
    working.release();
    assert_eq!(ended.load(Ordering::SeqCst), 1);
}

fn work(store: &LocalActivity) -> WorkHandle {
    store.begin(
        Begin {
            scope: "s".into(),
            agent: "a".into(),
            channel_id: "c".into(),
            message_id: "m".into(),
            thread_root_id: "m".into(),
        },
        1,
    )
}

#[tokio::test]
async fn the_work_row_settles_with_the_first_failure_any_hold_reports() {
    let store = LocalActivity::default();
    let working = Working::new(
        Vec::new(),
        Some(work(&store)),
        tokio::runtime::Handle::current(),
    );
    assert_eq!(store.work()[0].state, LocalWorkState::Working);
    working.until(async { WorkOutcome::failed("Claude was stopped") });
    settle().await;
    assert_eq!(store.work()[0].state, LocalWorkState::Working);
    working.release();
    let row = store.work()[0].clone();
    assert_eq!(row.state, LocalWorkState::Error);
    assert_eq!(row.error.as_deref(), Some("Claude was stopped"));
}

#[tokio::test]
async fn an_aborted_run_leaves_nothing_to_show() {
    let store = LocalActivity::default();
    let (ended, end) = counted();
    let working = Working::new(
        vec![end],
        Some(work(&store)),
        tokio::runtime::Handle::current(),
    );
    working.abort();
    assert!(store.work().is_empty());
    assert_eq!(ended.load(Ordering::SeqCst), 1);
    working.release();
    assert!(store.work().is_empty());
}

#[test]
fn types_in_the_thread_the_mention_belongs_to() {
    let event = |tags: Vec<Vec<&str>>| EventData {
        id: "m".repeat(64),
        pubkey: "a".repeat(64),
        created_at: 1,
        kind: 9,
        content: String::new(),
        tags: tags
            .into_iter()
            .map(|tag| tag.into_iter().map(str::to_owned).collect())
            .collect(),
    };
    let root = "r".repeat(64);
    let parent = "p".repeat(64);
    assert_eq!(thread_root(&event(vec![vec!["h", "c"]])), "m".repeat(64));
    assert_eq!(
        thread_root(&event(vec![vec!["e", &root, "", "reply"]])),
        root
    );
    assert_eq!(
        thread_root(&event(vec![
            vec!["e", &root, "", "root"],
            vec!["e", &parent, "", "reply"]
        ])),
        root
    );
    // A lone root marker is not a reply.
    assert_eq!(
        thread_root(&event(vec![vec!["e", &root, "", "root"]])),
        "m".repeat(64)
    );
}

#[tokio::test]
async fn shows_typing_from_the_first_hand_off_only() {
    let runtime = tokio::runtime::Handle::current();
    let begun = Arc::new(AtomicUsize::new(0));
    let (ended, end) = counted();
    let begin = {
        let begun = begun.clone();
        move || {
            begun.fetch_add(1, Ordering::SeqCst);
            end
        }
    };
    let working = Working::new(Vec::new(), None, runtime.clone()).on_first_hold(begin);
    working.until(async { WorkOutcome::ok() });
    working.until(async { WorkOutcome::ok() });
    working.release();
    settle().await;
    assert_eq!(begun.load(Ordering::SeqCst), 1);
    assert_eq!(ended.load(Ordering::SeqCst), 1);
    // A run that never hands off never shows it.
    let (never, end) = counted();
    let ignored = Working::new(Vec::new(), None, runtime).on_first_hold(move || end);
    ignored.release();
    settle().await;
    assert_eq!(never.load(Ordering::SeqCst), 0);
}
