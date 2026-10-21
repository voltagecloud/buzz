//! buzz-app's presence cases in `service.test.ts`.

use super::*;

const ORIGIN: &str = "https://relay.example";

fn online(agents: &[&str]) -> HashMap<String, BTreeSet<String>> {
    HashMap::from([(
        ORIGIN.to_string(),
        agents.iter().map(|agent| agent.to_string()).collect(),
    )])
}

fn bound(session: usize) -> Vec<(String, Option<usize>)> {
    vec![(ORIGIN.to_string(), Some(session))]
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| item.to_string()).collect()
}

#[test]
fn says_online_every_minute_on_a_new_session_and_offline_on_removal() {
    let mut announcer = Announcer::default();
    let start = Instant::now();
    let none = |_: &str| false;
    let first = announcer.plan(&bound(1), &online(&["a"]), none, start);
    assert_eq!(first.len(), 1);
    assert_eq!(
        (first[0].offline.len(), first[0].online.clone()),
        (0, strings(&["a"]))
    );
    // Nothing more until it is answered.
    assert!(
        announcer
            .plan(&bound(1), &online(&["a"]), none, start)
            .is_empty()
    );
    announcer.finished(ORIGIN, first[0].id, Ok(true));
    assert!(
        announcer
            .plan(
                &bound(1),
                &online(&["a"]),
                none,
                start + Duration::from_secs(30)
            )
            .is_empty()
    );
    let again = announcer.plan(&bound(1), &online(&["a"]), none, start + EVERY);
    assert_eq!(again[0].online, strings(&["a"]));
    announcer.finished(ORIGIN, again[0].id, Ok(true));
    // A new session hears it at once.
    let fresh = announcer.plan(&bound(2), &online(&["a"]), none, start + EVERY);
    assert_eq!(fresh.len(), 1);
    announcer.finished(ORIGIN, fresh[0].id, Ok(true));
    // Stopped: offline.
    let removed = announcer.plan(&bound(2), &online(&[]), none, start + EVERY);
    assert_eq!(
        removed,
        [Announcement {
            origin: ORIGIN.into(),
            id: removed[0].id,
            offline: strings(&["a"]),
            online: vec![],
        }]
    );
    announcer.finished(ORIGIN, removed[0].id, Ok(true));
    // Then nothing while it runs none.
    assert!(
        announcer
            .plan(&bound(2), &online(&[]), none, start + EVERY * 3)
            .is_empty()
    );
}

#[test]
fn an_agent_another_copy_took_over_is_not_said_offline() {
    let mut announcer = Announcer::default();
    let now = Instant::now();
    let sent = announcer.plan(&bound(1), &online(&["a", "b"]), |_| false, now);
    announcer.finished(ORIGIN, sent[0].id, Ok(true));
    let next = announcer.plan(&bound(1), &online(&["b"]), |agent| agent == "a", now);
    // Nothing changed for "b", and "a" is another copy's to announce.
    assert!(next.is_empty(), "{next:?}");
}

#[test]
fn a_refused_send_is_tried_again_after_thirty_seconds() {
    let mut announcer = Announcer::default();
    let start = Instant::now();
    let sent = announcer.plan(&bound(1), &online(&["a"]), |_| false, start);
    announcer.finished(ORIGIN, sent[0].id, Err("closed".into()));
    let soon = Instant::now();
    assert!(
        announcer
            .plan(&bound(1), &online(&["a"]), |_| false, soon)
            .is_empty()
    );
    let later = announcer.plan(&bound(1), &online(&["a"]), |_| false, soon + RETRY);
    assert_eq!(later.len(), 1);
    // A session that can never send it is not asked again.
    announcer.finished(ORIGIN, later[0].id, Ok(false));
    assert!(
        announcer
            .plan(&bound(1), &online(&["a"]), |_| false, soon + EVERY * 2)
            .is_empty()
    );
}
