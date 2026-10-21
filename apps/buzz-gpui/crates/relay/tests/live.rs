//! Live routes against a scripted fake relay: the test decides how every
//! REQ is answered (`live.ts` behaviour, deterministic).

use buzz_identity::Identity;
use buzz_relay_client::*;
use futures::{SinkExt, StreamExt};
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug)]
enum Seen {
    Connected,
    Req(String, Vec<Value>),
    Close(String),
}

struct Relay {
    address: String,
    seen: mpsc::UnboundedReceiver<Seen>,
    push: mpsc::UnboundedSender<Value>,
    kick: mpsc::UnboundedSender<()>,
}

impl Relay {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("ws://{}", listener.local_addr().unwrap());
        let (seen_tx, seen) = mpsc::unbounded_channel();
        let (push, mut push_rx) = mpsc::unbounded_channel::<Value>();
        let (kick, mut kick_rx) = mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                let _ = seen_tx.send(Seen::Connected);
                ws.send(Message::Text(json!(["AUTH", "c"]).to_string().into()))
                    .await
                    .unwrap();
                loop {
                    tokio::select! {
                        Some(()) = kick_rx.recv() => break,
                        Some(value) = push_rx.recv() => {
                            if ws.send(Message::Text(value.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                        message = ws.next() => {
                            let Some(Ok(Message::Text(text))) = message else { break };
                            let value: Vec<Value> = serde_json::from_str(text.as_str()).unwrap();
                            match value[0].as_str().unwrap() {
                                "AUTH" => {
                                    let event: Event = serde_json::from_value(value[1].clone()).unwrap();
                                    let ok = json!(["OK", event.id.to_hex(), true, ""]);
                                    ws.send(Message::Text(ok.to_string().into())).await.unwrap();
                                }
                                "REQ" => {
                                    let id = value[1].as_str().unwrap().to_string();
                                    let _ = seen_tx.send(Seen::Req(id, value[2..].to_vec()));
                                }
                                "CLOSE" => {
                                    let _ = seen_tx.send(Seen::Close(value[1].as_str().unwrap().into()));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        });
        Self {
            address,
            seen,
            push,
            kick,
        }
    }

    /// The next REQ, skipping connections and CLOSEs.
    async fn req(&mut self, live: &mut Live) -> (String, Vec<Value>) {
        loop {
            match self.next(live).await {
                Seen::Req(id, filters) => return (id, filters),
                _ => continue,
            }
        }
    }

    /// The next thing the relay saw, while driving `live`.
    async fn next(&mut self, live: &mut Live) -> Seen {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    seen = self.seen.recv() => return seen.unwrap(),
                    _ = live.next() => {}
                }
            }
        })
        .await
        .expect("the relay saw nothing")
    }

    /// Nothing reaches the relay within `wait` while `live` runs.
    async fn quiet(&mut self, live: &mut Live, wait: Duration) -> bool {
        tokio::time::timeout(wait, self.next(live)).await.is_err()
    }

    fn send(&self, value: Value) {
        self.push.send(value).unwrap();
    }
}

fn options() -> LiveOptions {
    LiveOptions {
        socket: SocketOptions {
            min_backoff: Duration::from_millis(20),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn connect(relay: &Relay, options: LiveOptions) -> Live {
    Live::connect(
        &tokio::runtime::Handle::current(),
        RelayOrigin::parse(&relay.address).unwrap(),
        Arc::new(Identity::ephemeral()),
        Arc::new(Verifier::default()),
        options,
    )
}

/// Drives `live` until `test` holds for its snapshot.
async fn until(live: &mut Live, test: impl Fn(&LiveSnapshot) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !test(&live.snapshot()) {
            live.next().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("snapshot never matched: {:?}", live.snapshot()));
}

/// Drives `live` until an update matches.
async fn update<T>(live: &mut Live, mut pick: impl FnMut(LiveUpdate) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(found) = pick(live.next().await) {
                return found;
            }
        }
    })
    .await
    .expect("update did not arrive")
}

fn route<'a>(snapshot: &'a LiveSnapshot, id: &str) -> Option<&'a LiveRoute> {
    snapshot.routes.iter().find(|route| route.id == id)
}

fn h(channel: &str) -> Vec<String> {
    vec!["h".into(), channel.into()]
}

fn event(keys: &Keys, tags: Vec<Vec<String>>, content: &str) -> Value {
    let event = EventBuilder::new(Kind::from_u16(9), content)
        .tags(tags.into_iter().map(|tag| Tag::parse(tag).unwrap()))
        .finalize(keys)
        .unwrap();
    serde_json::to_value(event).unwrap()
}

/// Connects and settles the two globals with EOSE.
async fn settled(relay: &mut Relay, options: LiveOptions) -> Live {
    let mut live = connect(relay, options);
    for _ in 0..2 {
        let (id, _) = relay.req(&mut live).await;
        relay.send(json!(["EOSE", id]));
    }
    until(&mut live, |s| {
        route(s, "profiles").is_some_and(|r| r.status == LiveRouteStatus::Live)
            && route(s, "membership").is_some_and(|r| r.status == LiveRouteStatus::Live)
    })
    .await;
    live
}

#[tokio::test]
async fn nothing_is_sent_before_authentication_and_globals_go_first() {
    let mut relay = Relay::start().await;
    let mut live = connect(&relay, options());
    assert_eq!(live.snapshot().status, LiveConnection::Connecting);
    live.update(["c1".to_string()], [], vec!["c1".into()])
        .unwrap();
    // Pending routes exist before the connection does.
    assert!(route(&live.snapshot(), "channel:c1").is_some());
    let mut kinds = Vec::new();
    for _ in 0..3 {
        let (_, filters) = relay.req(&mut live).await;
        kinds.push(filters[0]["kinds"].clone());
    }
    assert_eq!(kinds[0], json!([0, 10100, 30177]));
    assert_eq!(kinds[1], json!([44100, 44101]));
    assert!(kinds[2].as_array().unwrap().contains(&json!(9)));
    assert_eq!(live.snapshot().status, LiveConnection::Connected);
}

#[tokio::test]
async fn a_missing_quota_hint_is_five_seconds_and_retried_quietly() {
    let mut relay = Relay::start().await;
    let mut live = settled(&mut relay, options()).await;
    live.update(["c1".to_string()], [], Vec::new()).unwrap();
    let (id, _) = relay.req(&mut live).await;
    relay.send(json!([
        "CLOSED",
        id,
        "rate-limited: shared admission unavailable"
    ]));
    until(&mut live, |s| {
        route(s, "channel:c1").is_some_and(|r| {
            r.status == LiveRouteStatus::Pending
                && r.error.as_deref() == Some("rate-limited: shared admission unavailable")
        })
    })
    .await;
    let wait = live.cooldown().remaining();
    assert!(
        wait > Duration::from_millis(5_500) && wait <= Duration::from_secs(6),
        "{wait:?}"
    );
}

#[tokio::test]
async fn quota_retries_are_bounded_and_stop_the_unsent_queue() {
    let mut relay = Relay::start().await;
    let mut live = settled(
        &mut relay,
        LiveOptions {
            setup_concurrency: 1,
            ..options()
        },
    )
    .await;
    live.update(["c1".to_string(), "c2".to_string()], [], Vec::new())
        .unwrap();
    for attempt in 0..4 {
        let (id, filters) = relay.req(&mut live).await;
        assert_eq!(filters[0]["#h"], json!(["c1"]), "attempt {attempt}");
        relay.send(json!([
            "CLOSED",
            id,
            "rate-limited: quota exceeded; retry in 0s"
        ]));
    }
    until(&mut live, |s| {
        route(s, "channel:c1").is_some_and(|r| r.status == LiveRouteStatus::Error)
            && route(s, "channel:c2").is_some_and(|r| {
                r.status == LiveRouteStatus::Error
                    && r.error.as_deref()
                        == Some("Live request cooldown retries exhausted; retry available")
            })
    })
    .await;
    assert!(relay.quiet(&mut live, Duration::from_millis(1_500)).await);

    // Retry keeps the relay's cooldown, then sends both again.
    live.retry();
    let (_, filters) = relay.req(&mut live).await;
    assert_eq!(filters[0]["#h"], json!(["c1"]));
}

#[tokio::test]
async fn an_unsupported_cooldown_stops_automatic_setup() {
    let mut relay = Relay::start().await;
    let mut live = settled(
        &mut relay,
        LiveOptions {
            setup_concurrency: 1,
            ..options()
        },
    )
    .await;
    live.update(["c1".to_string(), "c2".to_string()], [], Vec::new())
        .unwrap();
    let (id, _) = relay.req(&mut live).await;
    relay.send(json!([
        "CLOSED",
        id,
        "rate-limited: quota exceeded; retry in 120s"
    ]));
    until(&mut live, |s| {
        route(s, "channel:c2").is_some_and(|r| {
            r.error.as_deref() == Some("Unsupported live cooldown; automatic setup stopped")
        })
    })
    .await;
    let wait = live.cooldown().remaining();
    assert!(
        wait > Duration::from_secs(120),
        "paused the whole hint: {wait:?}"
    );
    // The user's retry keeps it.
    live.retry();
    assert!(live.cooldown().remaining() > Duration::from_secs(119));
    assert!(relay.quiet(&mut live, Duration::from_millis(300)).await);
}

#[tokio::test]
async fn denial_and_invalid_traffic_fail_their_route() {
    let mut relay = Relay::start().await;
    let mut live = settled(&mut relay, options()).await;
    live.update(["c1".to_string(), "c2".to_string()], [], Vec::new())
        .unwrap();
    let (first, filters) = relay.req(&mut live).await;
    let (second, _) = relay.req(&mut live).await;
    let (denied, invalid) = if filters[0]["#h"] == json!(["c1"]) {
        (first, second)
    } else {
        (second, first)
    };
    relay.send(json!([
        "CLOSED",
        denied,
        "restricted: not a channel member"
    ]));
    let channels = update(&mut live, |u| match u {
        LiveUpdate::Denied { channels, .. } => Some(channels),
        _ => None,
    })
    .await;
    assert_eq!(channels, ["c1"]);

    let mut forged = event(&Keys::generate(), vec![h("c2")], "x");
    forged["content"] = json!("tampered");
    relay.send(json!(["EVENT", invalid, forged]));
    until(&mut live, |s| {
        route(s, "channel:c2")
            .is_some_and(|r| r.error.as_deref() == Some("Relay supplied invalid live traffic"))
    })
    .await;
}

#[tokio::test]
async fn attribution_and_replay_limits() {
    let mut relay = Relay::start().await;
    let mut live = settled(
        &mut relay,
        LiveOptions {
            replay_limit: 2,
            ..options()
        },
    )
    .await;
    let joined = ["a".to_string(), "b".to_string()];
    live.update(joined.clone(), joined.clone(), Vec::new())
        .unwrap();
    let (id, filters) = relay.req(&mut live).await;
    assert_eq!(filters.len(), 2, "one filter per channel on first setup");
    let keys = Keys::generate();
    relay.send(json!(["EVENT", id, event(&keys, vec![h("a")], "one")]));
    relay.send(json!([
        "EVENT",
        id,
        event(&keys, vec![h("a"), h("b")], "both")
    ]));
    relay.send(json!([
        "EVENT",
        id,
        event(&keys, vec![h("z")], "elsewhere")
    ]));
    relay.send(json!(["EOSE", id]));
    let one = update(&mut live, |u| match u {
        LiveUpdate::Events {
            events,
            phase,
            channel,
            ..
        } if events[0].content == "one" => Some((phase, channel)),
        _ => None,
    })
    .await;
    assert_eq!(one, (Phase::Replay, Some("a".to_string())));
    let both = update(&mut live, |u| match u {
        LiveUpdate::Events {
            events,
            channel,
            sources,
            ..
        } if events[0].content == "both" => Some((channel, sources)),
        LiveUpdate::Events { events, .. } if events[0].content == "elsewhere" => {
            panic!("an event for another channel leaked")
        }
        _ => None,
    })
    .await;
    assert_eq!(both.0, None, "two channels: no single attribution");
    assert_eq!(&*both.1, ["a".to_string(), "b".to_string()]);
    until(&mut live, |s| {
        route(s, "channel:a")
            .is_some_and(|r| r.status == LiveRouteStatus::Live && r.replay == Replay::Limited)
    })
    .await;
}

#[tokio::test]
async fn a_narrowed_batch_filters_its_old_wire_to_the_new_scope() {
    let mut relay = Relay::start().await;
    let mut live = settled(&mut relay, options()).await;
    let joined = ["a".to_string(), "b".to_string()];
    live.update(joined.clone(), joined.clone(), Vec::new())
        .unwrap();
    let (old, _) = relay.req(&mut live).await;
    relay.send(json!(["EOSE", old]));
    until(&mut live, |s| {
        route(s, "channel:a").is_some_and(|r| r.status == LiveRouteStatus::Live)
    })
    .await;

    live.update(["a".to_string()], ["a".to_string()], Vec::new())
        .unwrap();
    let (new, filters) = relay.req(&mut live).await;
    assert_eq!(filters, vec![filters[0].clone()]);
    assert_eq!(filters[0]["#h"], json!(["a"]));
    assert_eq!(filters[0]["limit"], json!(0), "replacements are live-only");
    let keys = Keys::generate();
    relay.send(json!(["EVENT", old, event(&keys, vec![h("b")], "retired")]));
    relay.send(json!(["EVENT", old, event(&keys, vec![h("a")], "kept")]));
    let content = update(&mut live, |u| match u {
        LiveUpdate::Events { events, .. } => Some(events[0].content.clone()),
        _ => None,
    })
    .await;
    assert_eq!(content, "kept");
    // EOSE on the replacement closes the old wire.
    relay.send(json!(["EOSE", new]));
    loop {
        if let Seen::Close(closed) = relay.next(&mut live).await {
            assert_eq!(closed, old);
            break;
        }
    }
}

#[tokio::test]
async fn a_timed_out_renewal_keeps_its_wire_and_retries_on_the_tick() {
    let mut relay = Relay::start().await;
    let mut live = settled(
        &mut relay,
        LiveOptions {
            setup_deadline: Duration::from_millis(300),
            recovery_interval: Duration::from_millis(800),
            ..options()
        },
    )
    .await;
    let joined = ["a".to_string(), "b".to_string()];
    live.update(joined.clone(), joined.clone(), Vec::new())
        .unwrap();
    let (old, _) = relay.req(&mut live).await;
    relay.send(json!(["EOSE", old]));
    // The minute tick renews the batch; the renewal never answers.
    let (renewal, filters) = relay.req(&mut live).await;
    assert_eq!(filters[0]["limit"], json!(0));
    until(&mut live, |s| {
        route(s, "channel:a").is_some_and(|r| {
            r.error.as_deref() == Some("Live subscription setup timed out; retry available")
        })
    })
    .await;
    // The old wire still delivers, and the next tick tries again.
    let keys = Keys::generate();
    relay.send(json!([
        "EVENT",
        old,
        event(&keys, vec![h("a")], "still here")
    ]));
    update(&mut live, |u| match u {
        LiveUpdate::Events { events, .. } => (events[0].content == "still here").then_some(()),
        _ => None,
    })
    .await;
    let (again, _) = relay.req(&mut live).await;
    assert_ne!(again, renewal);
    update(&mut live, |u| {
        matches!(u, LiveUpdate::Recover).then_some(())
    })
    .await;
}

#[tokio::test]
async fn interests_are_validated_whole() {
    let relay = Relay::start().await;
    let mut live = connect(&relay, options());
    assert_eq!(
        live.update(["bad id".to_string()], [], Vec::new()),
        Err("Invalid live channel interests (maximum 1024 bounded IDs)".into())
    );
    assert_eq!(live.snapshot().status, LiveConnection::Error);
    assert_eq!(
        live.update(["a".to_string()], ["b".to_string()], Vec::new()),
        Err("Joined scope outside live interests".into())
    );
    assert_eq!(
        live.watch_presence(vec!["ab".into()]),
        Err("Invalid live presence authors".into())
    );
    live.update(["a".to_string()], [], Vec::new()).unwrap();
    assert_eq!(live.snapshot().error, None);
}

#[tokio::test]
async fn a_dropped_connection_retries_and_reports_why() {
    let mut relay = Relay::start().await;
    let mut live = settled(&mut relay, options()).await;
    relay.kick.send(()).unwrap();
    update(&mut live, |u| {
        matches!(u, LiveUpdate::Disconnected).then_some(())
    })
    .await;
    until(&mut live, |s| s.status == LiveConnection::Connected).await;
    // The routes came back with a fresh look-back.
    let (_, filters) = relay.req(&mut live).await;
    assert!(filters[0]["since"].as_u64().is_some());
}
