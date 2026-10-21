//! buzz-app's `typing.test.ts`.

use super::*;
use futures::FutureExt;

fn event(id: &str, tags: &[&[&str]]) -> EventData {
    EventData {
        id: id.into(),
        tags: tags
            .iter()
            .map(|tag| tag.iter().map(|part| part.to_string()).collect())
            .collect(),
        ..Default::default()
    }
}

/// Records each pulse; fails while `failing`.
#[derive(Clone, Default)]
struct Sent {
    pulses: Arc<Mutex<Vec<AppAgentEvent>>>,
    failing: Arc<Mutex<bool>>,
}

impl Sent {
    fn publish(&self) -> Publish {
        let this = self.clone();
        Arc::new(move |event| {
            this.pulses.lock().unwrap().push(event);
            let failing = *this.failing.lock().unwrap();
            async move {
                if failing {
                    Err("refused".into())
                } else {
                    Ok(())
                }
            }
            .boxed()
        })
    }
    fn count(&self) -> usize {
        self.pulses.lock().unwrap().len()
    }
}

fn answer(flat: Result<bool, String>) -> BoxFuture<'static, Result<bool, String>> {
    async move { flat }.boxed()
}

async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn pulses_in_the_mentions_thread_or_the_channel_itself() {
    let reply = event("e2", &[&["h", "chan"], &["e", "e1", "", "reply"]]);
    assert_eq!(
        template(&reply, "chan", false).tags,
        vec![
            vec!["h".to_string(), "chan".into()],
            vec!["e".into(), "e1".into(), String::new(), "root".into()],
            vec!["e".into(), "e2".into(), String::new(), "reply".into()],
        ]
    );
    let top = event("e1", &[&["h", "chan"]]);
    assert_eq!(
        template(&top, "chan", false).tags,
        vec![
            vec!["h".to_string(), "chan".into()],
            vec!["e".into(), "e1".into(), String::new(), "reply".into()],
        ]
    );
    let flat = template(&reply, "chan", true);
    assert_eq!((flat.kind, flat.tags.len()), (20002, 1));
}

#[tokio::test(start_paused = true)]
async fn turns_in_one_conversation_share_a_pulse_until_the_last_stops() {
    let sent = Sent::default();
    let pulses = Pulses::default();
    let runtime = tokio::runtime::Handle::current();
    let mention = event("e1", &[&["h", "chan"]]);
    let first = pulse_typing(
        sent.publish(),
        answer(Ok(false)),
        &mention,
        "chan",
        pulses.clone(),
        &runtime,
    );
    let second = pulse_typing(
        sent.publish(),
        answer(Ok(false)),
        &mention,
        "chan",
        pulses.clone(),
        &runtime,
    );
    settle().await;
    assert_eq!((pulses.running(), sent.count()), (1, 1));
    tokio::time::sleep(PULSE).await;
    settle().await;
    assert_eq!(sent.count(), 2);
    first();
    tokio::time::sleep(PULSE).await;
    settle().await;
    assert_eq!(sent.count(), 3, "the other turn still types");
    second();
    tokio::time::sleep(PULSE * 3).await;
    settle().await;
    assert_eq!((pulses.running(), sent.count()), (0, 3));
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_cannot_tell_where_to_answer_shows_no_typing() {
    let sent = Sent::default();
    let pulses = Pulses::default();
    let runtime = tokio::runtime::Handle::current();
    let mention = event("e1", &[&["h", "chan"]]);
    let end = pulse_typing(
        sent.publish(),
        answer(Err("unread".into())),
        &mention,
        "chan",
        pulses.clone(),
        &runtime,
    );
    settle().await;
    assert_eq!(sent.count(), 0);
    end();
    // Ended before its answer: it never starts.
    let (tell, told) = futures::channel::oneshot::channel::<Result<bool, String>>();
    let end = pulse_typing(
        sent.publish(),
        async move { told.await.unwrap() }.boxed(),
        &mention,
        "chan",
        pulses.clone(),
        &runtime,
    );
    end();
    tell.send(Ok(true)).unwrap();
    settle().await;
    assert_eq!((pulses.running(), sent.count()), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn rides_out_a_failure_then_stops_after_three_in_a_row() {
    let sent = Sent::default();
    let pulses = Pulses::default();
    let runtime = tokio::runtime::Handle::current();
    let mention = event("e1", &[&["h", "chan"]]);
    let _end = pulse_typing(
        sent.publish(),
        answer(Ok(false)),
        &mention,
        "chan",
        pulses.clone(),
        &runtime,
    );
    settle().await;
    *sent.failing.lock().unwrap() = true;
    tokio::time::sleep(PULSE).await;
    settle().await;
    *sent.failing.lock().unwrap() = false;
    tokio::time::sleep(PULSE).await;
    settle().await;
    assert_eq!(pulses.running(), 1, "one blip is ridden out");
    *sent.failing.lock().unwrap() = true;
    tokio::time::sleep(PULSE * 3).await;
    settle().await;
    let count = sent.count();
    assert_eq!(pulses.running(), 0);
    tokio::time::sleep(PULSE * 3).await;
    settle().await;
    assert_eq!(sent.count(), count, "no more pulses");
}

#[tokio::test(start_paused = true)]
async fn stops_after_thirty_minutes() {
    let sent = Sent::default();
    let pulses = Pulses::default();
    let runtime = tokio::runtime::Handle::current();
    let mention = event("e1", &[&["h", "chan"]]);
    let _end = pulse_typing(
        sent.publish(),
        answer(Ok(false)),
        &mention,
        "chan",
        pulses.clone(),
        &runtime,
    );
    settle().await;
    tokio::time::sleep(LONGEST + PULSE).await;
    settle().await;
    assert_eq!(pulses.running(), 0);
}
