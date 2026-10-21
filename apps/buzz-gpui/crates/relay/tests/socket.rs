use buzz_identity::Identity;
use buzz_relay_client::*;
use futures::{SinkExt, StreamExt};
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, ToBech32};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// What the fake relay saw from the client.
#[derive(Debug)]
enum Seen {
    Connected,
    Auth(Event),
    Req(String, Vec<Value>),
    Close(String),
    Event(Event),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    /// No NIP-42: everything is allowed at once.
    Open,
    /// Challenge on connect and accept a valid answer.
    Accept,
    /// Challenge on connect and refuse every answer.
    Refuse,
    /// Never challenge, and refuse REQs.
    Silent,
}

#[derive(Clone, Copy)]
struct Behaviour {
    auth: Auth,
    /// Answer publications with `OK`; when off they wait forever.
    ack: bool,
    /// Close every connection right after accepting it.
    drop_all: bool,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            auth: Auth::Accept,
            ack: true,
            drop_all: false,
        }
    }
}

struct FakeRelay {
    address: String,
    seen: mpsc::UnboundedReceiver<Seen>,
    /// Messages to push to the connected client.
    push: mpsc::UnboundedSender<Value>,
    /// Drops the current connection when sent.
    kick: mpsc::UnboundedSender<()>,
}

impl FakeRelay {
    async fn start(behaviour: Behaviour) -> Self {
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
                if behaviour.drop_all {
                    let _ = ws.close(None).await;
                    continue;
                }
                let mut authed = behaviour.auth == Auth::Open;
                if matches!(behaviour.auth, Auth::Accept | Auth::Refuse) {
                    ws.send(Message::Text(
                        json!(["AUTH", "challenge-1"]).to_string().into(),
                    ))
                    .await
                    .unwrap();
                }
                loop {
                    tokio::select! {
                        Some(()) = kick_rx.recv() => break,
                        Some(value) = push_rx.recv() => {
                            ws.send(Message::Text(value.to_string().into())).await.unwrap();
                        }
                        message = ws.next() => {
                            let Some(Ok(Message::Text(text))) = message else { break };
                            let value: Vec<Value> = serde_json::from_str(text.as_str()).unwrap();
                            match value[0].as_str().unwrap() {
                                "AUTH" => {
                                    let event: Event = serde_json::from_value(value[1].clone()).unwrap();
                                    authed = behaviour.auth == Auth::Accept && event.verify().is_ok();
                                    let ok = json!(["OK", event.id.to_hex(), authed, ""]);
                                    let _ = seen_tx.send(Seen::Auth(event));
                                    ws.send(Message::Text(ok.to_string().into())).await.unwrap();
                                }
                                "REQ" => {
                                    let id = value[1].as_str().unwrap().to_string();
                                    if !authed {
                                        let closed = json!(["CLOSED", id, "auth-required: sign in"]);
                                        ws.send(Message::Text(closed.to_string().into())).await.unwrap();
                                        continue;
                                    }
                                    let _ = seen_tx.send(Seen::Req(id.clone(), value[2..].to_vec()));
                                    ws.send(Message::Text(json!(["EOSE", id]).to_string().into()))
                                        .await
                                        .unwrap();
                                }
                                "CLOSE" => {
                                    let _ = seen_tx.send(Seen::Close(value[1].as_str().unwrap().into()));
                                }
                                "EVENT" => {
                                    let event: Event = serde_json::from_value(value[1].clone()).unwrap();
                                    let reason = match event.content.as_str() {
                                        "reject me" => Some("blocked: no"),
                                        "busy" => Some("rate-limited: quota exceeded; retry in 3s"),
                                        "oops" => Some("error: internal server error"),
                                        _ => None,
                                    };
                                    let ok = json!(["OK", event.id.to_hex(), reason.is_none(), reason.unwrap_or("")]);
                                    let _ = seen_tx.send(Seen::Event(event));
                                    if behaviour.ack {
                                        ws.send(Message::Text(ok.to_string().into())).await.unwrap();
                                    }
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

    async fn next(&mut self) -> Seen {
        tokio::time::timeout(Duration::from_secs(5), self.seen.recv())
            .await
            .expect("relay saw nothing")
            .unwrap()
    }

    /// Nothing more arrives within `wait`.
    async fn quiet(&mut self, wait: Duration) -> bool {
        tokio::time::timeout(wait, self.seen.recv()).await.is_err()
    }

    /// How many events arrive before it has been quiet for `wait`.
    async fn events_until_quiet(&mut self, wait: Duration) -> usize {
        let mut events = 0;
        while let Ok(seen) = tokio::time::timeout(wait, self.seen.recv()).await {
            match seen.unwrap() {
                Seen::Event(_) => events += 1,
                _ => panic!("expected only events"),
            }
        }
        events
    }
}

/// No jitter: every delay is its full ceiling.
fn options() -> SocketOptions {
    SocketOptions {
        min_backoff: Duration::from_millis(20),
        publish_timeout: Duration::from_secs(2),
        jitter: || 1.0,
        ..Default::default()
    }
}

/// An identity and the same key for signing test events.
fn viewer() -> (Arc<Identity>, Keys) {
    let keys = Keys::generate();
    let identity = Identity::from_nsec(&keys.secret_key().to_bech32().unwrap()).unwrap();
    (Arc::new(identity), keys)
}

fn connect(relay: &FakeRelay, identity: &Arc<Identity>, options: SocketOptions) -> RelaySocket {
    RelaySocket::connect(
        &tokio::runtime::Handle::current(),
        RelayOrigin::parse(&relay.address).unwrap(),
        identity.clone(),
        Arc::new(Verifier::default()),
        options,
    )
}

fn socket(relay: &FakeRelay, identity: &Arc<Identity>) -> RelaySocket {
    connect(relay, identity, options())
}

async fn next_event(subscription: &mut Subscription) -> SubscriptionEvent {
    tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .expect("no subscription event")
        .expect("subscription ended")
}

async fn wait_state(socket: &RelaySocket, test: impl Fn(&SocketState) -> bool) -> SocketState {
    let mut state = socket.state();
    tokio::time::timeout(Duration::from_secs(10), state.wait_for(test))
        .await
        .expect("state never reached")
        .unwrap()
        .clone()
}

fn filter(value: Value) -> Filter {
    value.as_object().unwrap().clone()
}

fn note(keys: &Keys, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .finalize(keys)
        .unwrap()
}

#[tokio::test]
async fn authenticates_before_subscribing() {
    let mut relay = FakeRelay::start(Behaviour::default()).await;
    let (identity, _) = viewer();
    let socket = socket(&relay, &identity);
    let mut subscription = socket.subscribe(vec![filter(json!({"kinds": [9], "#h": ["general"]}))]);

    assert!(matches!(relay.next().await, Seen::Connected));
    let Seen::Auth(auth) = relay.next().await else {
        panic!("expected auth first");
    };
    assert_eq!(auth.kind, Kind::Authentication);
    assert_eq!(auth.pubkey, identity.public_key());
    let tags: Vec<Vec<String>> = auth.tags.iter().map(|t| t.as_slice().to_vec()).collect();
    assert!(tags.contains(&vec!["challenge".into(), "challenge-1".into()]));
    assert!(tags.contains(&vec!["relay".into(), relay.address.clone()]));

    let Seen::Req(_, filters) = relay.next().await else {
        panic!("expected the held REQ after auth");
    };
    assert_eq!(filters, vec![json!({"kinds": [9], "#h": ["general"]})]);
    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Eose
    ));
    wait_state(&socket, |s| *s == SocketState::Ready).await;
}

#[tokio::test]
async fn a_relay_that_never_asks_is_never_treated_as_open() {
    let mut relay = FakeRelay::start(Behaviour {
        auth: Auth::Silent,
        ..Default::default()
    })
    .await;
    let (identity, keys) = viewer();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            auth_deadline: Duration::from_millis(300),
            max_reconnects: Some(0),
            ..options()
        },
    );
    let _subscription = socket.subscribe(vec![filter(json!({"kinds": [9]}))]);
    let publication = tokio::spawn({
        let publisher = socket.publisher();
        let event = note(&keys, "held");
        async move { publisher.publish(event).await }
    });
    assert!(matches!(relay.next().await, Seen::Connected));
    // The deadline runs from the start of the attempt; nothing is sent.
    let failed = wait_state(&socket, |s| matches!(s, SocketState::Failed { .. })).await;
    assert_eq!(
        failed,
        SocketState::Failed {
            error: "Live reconnect attempts exhausted; retry available".into()
        }
    );
    assert_eq!(
        publication.await.unwrap(),
        Err(PublishError::NotSent(
            "Socket connection interrupted".into()
        ))
    );
    // Queued while stopped, it waits out its own deadline.
    assert_eq!(
        socket.publish(note(&keys, "later")).await,
        Err(PublishError::NotSent("Publication timed out".into()))
    );
    assert!(
        relay.quiet(Duration::from_millis(300)).await,
        "no REQ or EVENT"
    );
}

#[tokio::test]
async fn a_refused_authentication_is_terminal_until_retried() {
    let mut relay = FakeRelay::start(Behaviour {
        auth: Auth::Refuse,
        ..Default::default()
    })
    .await;
    let (identity, _) = viewer();
    let socket = socket(&relay, &identity);
    let _subscription = socket.subscribe(vec![filter(json!({"kinds": [9]}))]);
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));
    assert_eq!(
        wait_state(&socket, |s| matches!(s, SocketState::Failed { .. })).await,
        SocketState::Failed {
            error: "Relay rejected live authentication".into()
        }
    );
    assert!(
        relay.quiet(Duration::from_millis(300)).await,
        "no reconnect without a retry"
    );
    socket.reconnect();
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));
}

#[tokio::test]
async fn reconnects_back_off_and_stop_after_a_set_limit() {
    let mut relay = FakeRelay::start(Behaviour {
        drop_all: true,
        ..Default::default()
    })
    .await;
    let (identity, _) = viewer();
    let started = tokio::time::Instant::now();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            max_reconnects: Some(5),
            ..options()
        },
    );
    let mut waits = Vec::new();
    let mut state = socket.state();
    loop {
        let current = tokio::time::timeout(Duration::from_secs(10), state.changed())
            .await
            .expect("socket stalled");
        current.unwrap();
        match state.borrow_and_update().clone() {
            SocketState::Waiting { retry_in, error } => {
                assert_eq!(error, "Live connection closed");
                waits.push(retry_in);
            }
            SocketState::Failed { error } => {
                assert_eq!(error, "Live reconnect attempts exhausted; retry available");
                break;
            }
            _ => {}
        }
    }
    // 500 ms · 2ⁿ (here 20 ms · 2ⁿ), without jitter, five times.
    let expected: Vec<Duration> = (0..5).map(|n| Duration::from_millis(20 << n)).collect();
    assert_eq!(waits, expected);
    assert!(started.elapsed() >= Duration::from_millis(620));
    let mut connections = 0;
    while let Ok(Some(seen)) =
        tokio::time::timeout(Duration::from_millis(200), relay.seen.recv()).await
    {
        assert!(matches!(seen, Seen::Connected));
        connections += 1;
    }
    assert_eq!(connections, 6, "the first attempt and five reconnects");

    // A retry starts a fresh budget at once.
    socket.reconnect();
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(
        wait_state(&socket, |s| matches!(s, SocketState::Waiting { .. })).await,
        SocketState::Waiting { retry_in, .. } if retry_in == Duration::from_millis(20)
    ));
}

async fn authenticate(relay: &mut FakeRelay, socket: &RelaySocket) {
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));
    wait_state(socket, |s| *s == SocketState::Ready).await;
}

#[tokio::test]
async fn only_a_healthy_connection_restores_prompt_reconnects() {
    let mut relay = FakeRelay::start(Behaviour::default()).await;
    let (identity, _) = viewer();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            healthy_after: Duration::from_millis(300),
            ..options()
        },
    );
    let waits = |socket: &RelaySocket| {
        let state = socket.state();
        async move {
            let mut state = state;
            let found = tokio::time::timeout(
                Duration::from_secs(10),
                state.wait_for(|s| matches!(s, SocketState::Waiting { .. })),
            )
            .await
            .expect("never waited")
            .unwrap()
            .clone();
            match found {
                SocketState::Waiting { retry_in, .. } => retry_in,
                _ => unreachable!(),
            }
        }
    };
    authenticate(&mut relay, &socket).await;
    relay.kick.send(()).unwrap();
    assert_eq!(waits(&socket).await, Duration::from_millis(20));
    // Authenticating alone does not reset the backoff of a flapping relay.
    authenticate(&mut relay, &socket).await;
    relay.kick.send(()).unwrap();
    assert_eq!(waits(&socket).await, Duration::from_millis(40));
    // A healthy stretch authenticated does, for the next rollout.
    authenticate(&mut relay, &socket).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    relay.kick.send(()).unwrap();
    assert_eq!(waits(&socket).await, Duration::from_millis(20));
}

#[tokio::test]
async fn reconnect_delays_spread_over_half_to_all_of_the_ceiling() {
    let relay = FakeRelay::start(Behaviour {
        drop_all: true,
        ..Default::default()
    })
    .await;
    let (identity, _) = viewer();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            jitter: || 0.0,
            max_backoff: Duration::from_millis(80),
            ..options()
        },
    );
    let mut waits = Vec::new();
    let mut state = socket.state();
    while waits.len() < 5 {
        tokio::time::timeout(Duration::from_secs(10), state.changed())
            .await
            .expect("socket stalled")
            .unwrap();
        if let SocketState::Waiting { retry_in, .. } = state.borrow_and_update().clone() {
            waits.push(retry_in);
        }
    }
    let expected: Vec<Duration> = [10, 20, 40, 40, 40]
        .into_iter()
        .map(Duration::from_millis)
        .collect();
    assert_eq!(waits, expected);
}

#[tokio::test]
async fn reconnects_keep_going_with_a_capped_backoff() {
    let relay = FakeRelay::start(Behaviour {
        drop_all: true,
        ..Default::default()
    })
    .await;
    let (identity, _) = viewer();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            max_backoff: Duration::from_millis(80),
            ..options()
        },
    );
    let mut waits = Vec::new();
    let mut state = socket.state();
    while waits.len() < 7 {
        tokio::time::timeout(Duration::from_secs(10), state.changed())
            .await
            .expect("socket stalled")
            .unwrap();
        match state.borrow_and_update().clone() {
            SocketState::Waiting { retry_in, .. } => waits.push(retry_in),
            SocketState::Failed { error } => panic!("gave up: {error}"),
            _ => {}
        }
    }
    let expected: Vec<Duration> = [20, 40, 80, 80, 80, 80, 80]
        .into_iter()
        .map(Duration::from_millis)
        .collect();
    assert_eq!(waits, expected);
}

#[tokio::test]
async fn invalid_events_are_reported_and_valid_ones_delivered() {
    let mut relay = FakeRelay::start(Behaviour {
        auth: Auth::Open,
        ..Default::default()
    })
    .await;
    let (identity, _) = viewer();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            open_after: Some(Duration::from_millis(100)),
            ..options()
        },
    );
    let mut subscription = socket.subscribe(vec![filter(json!({"kinds": [1]}))]);
    assert!(matches!(relay.next().await, Seen::Connected));
    let Seen::Req(id, _) = relay.next().await else {
        panic!("expected REQ");
    };
    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Eose
    ));

    let author = Keys::generate();
    let good = note(&author, "hello");
    let mut forged = serde_json::to_value(note(&author, "original")).unwrap();
    forged["content"] = json!("tampered");
    relay.push.send(json!(["EVENT", id, forged])).unwrap();
    // A frame over 1 MiB is dropped unread.
    let huge = note(&author, &"x".repeat(1024 * 1024));
    relay.push.send(json!(["EVENT", id, huge])).unwrap();
    relay.push.send(json!(["EVENT", id, good])).unwrap();

    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Invalid
    ));
    let SubscriptionEvent::Event(event) = next_event(&mut subscription).await else {
        panic!("expected an event");
    };
    assert_eq!(event.content, "hello", "the oversized frame was dropped");

    drop(subscription);
    assert!(matches!(relay.next().await, Seen::Close(closed) if closed == id));
}

#[tokio::test]
async fn publications_map_receipts_and_quota_pauses_dispatch() {
    let mut relay = FakeRelay::start(Behaviour::default()).await;
    let (identity, keys) = viewer();
    let socket = socket(&relay, &identity);
    wait_state(&socket, |s| *s == SocketState::Ready).await;
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));

    assert_eq!(socket.publish(note(&keys, "hi")).await, Ok(String::new()));
    assert!(matches!(relay.next().await, Seen::Event(e) if e.content == "hi"));
    assert_eq!(
        socket.publish(note(&keys, "reject me")).await,
        Err(PublishError::Rejected("blocked: no".into()))
    );
    assert!(matches!(relay.next().await, Seen::Event(_)));
    // An internal error may follow a side effect: not proof of failure.
    assert_eq!(
        socket.publish(note(&keys, "oops")).await,
        Err(PublishError::Unconfirmed {
            message: "Relay publication not confirmed".into(),
            refusal: Some("error: internal server error".into()),
        })
    );
    assert!(matches!(relay.next().await, Seen::Event(_)));

    // Someone else's event is never published as ours.
    let stranger = note(&Keys::generate(), "not mine");
    assert_eq!(
        socket.publish(stranger).await,
        Err(PublishError::NotSent(
            "Publication signer does not match viewer".into()
        ))
    );

    // A quota refusal pauses everything sent on this socket for N+1 s.
    assert!(matches!(
        socket.publish(note(&keys, "busy")).await,
        Err(PublishError::Rejected(reason)) if reason.starts_with("rate-limited:")
    ));
    assert!(matches!(relay.next().await, Seen::Event(_)));
    let held = tokio::spawn({
        let publisher = socket.publisher();
        let event = note(&keys, "after the pause");
        async move { publisher.publish(event).await }
    });
    assert!(
        relay.quiet(Duration::from_millis(500)).await,
        "held by the cooldown"
    );
    drop(held);
}

#[tokio::test]
async fn publication_limits_and_timeouts() {
    let mut relay = FakeRelay::start(Behaviour {
        ack: false,
        ..Default::default()
    })
    .await;
    let (identity, keys) = viewer();
    let socket = socket(&relay, &identity);
    wait_state(&socket, |s| *s == SocketState::Ready).await;
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));

    let event = note(&keys, "once");
    let first = tokio::spawn({
        let publisher = socket.publisher();
        let event = event.clone();
        async move { publisher.publish(event).await }
    });
    assert!(matches!(relay.next().await, Seen::Event(_)));
    assert_eq!(
        socket.publish(event).await,
        Err(PublishError::NotSent(
            "Publication already in flight".into()
        ))
    );
    let big = note(&keys, &"x".repeat(64 * 1024));
    assert_eq!(
        socket.publish(big).await,
        Err(PublishError::NotSent(
            "Publication exceeds frame limit".into()
        ))
    );

    // At most three wait for a receipt; the rest stay unsent.
    let mut rest = Vec::new();
    for n in 0..31 {
        let publisher = socket.publisher();
        let event = note(&keys, &format!("n{n}"));
        rest.push(tokio::spawn(async move { publisher.publish(event).await }));
    }
    for _ in 0..2 {
        assert!(matches!(relay.next().await, Seen::Event(_)));
    }
    assert!(
        relay.quiet(Duration::from_millis(300)).await,
        "three unacked"
    );
    // 32 are queued now.
    assert_eq!(
        socket.publish(note(&keys, "one too many")).await,
        Err(PublishError::NotSent("Publication capacity reached".into()))
    );

    // The timeout counts from enqueue; sent ones are unconfirmed.
    assert_eq!(
        first.await.unwrap(),
        Err(PublishError::Unconfirmed {
            message: "Publication timed out".into(),
            refusal: None,
        })
    );
    let mut not_sent = 0;
    let mut unconfirmed = 0;
    for publication in rest {
        match publication.await.unwrap() {
            Err(PublishError::NotSent(message)) => {
                assert_eq!(message, "Publication timed out");
                not_sent += 1;
            }
            Err(PublishError::Unconfirmed { message, refusal }) => {
                assert_eq!((message.as_str(), refusal), ("Publication timed out", None));
                unconfirmed += 1;
            }
            other => panic!("{other:?}"),
        }
    }
    // Each timeout frees a slot, and the next queued one may go out before
    // its own deadline; how many do depends on how far apart they were
    // queued. Whatever the count, exactly the ones reported sent reached
    // the relay, and the rest never left.
    assert!(not_sent > 0, "every queued publication was sent");
    assert!(unconfirmed >= 2, "{unconfirmed}");
    assert_eq!(
        relay.events_until_quiet(Duration::from_millis(300)).await,
        unconfirmed - 2,
        "{not_sent} not sent, {unconfirmed} unconfirmed"
    );
}

#[tokio::test]
async fn resubscribes_after_reconnect_and_fails_publishes_on_loss() {
    let mut relay = FakeRelay::start(Behaviour {
        ack: false,
        ..Default::default()
    })
    .await;
    let (identity, keys) = viewer();
    let socket = socket(&relay, &identity);
    let mut subscription = socket.subscribe(vec![filter(json!({"kinds": [9]}))]);
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));
    let Seen::Req(first_id, _) = relay.next().await else {
        panic!("expected REQ");
    };
    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Eose
    ));
    let publication = tokio::spawn({
        let publisher = socket.publisher();
        let event = note(&keys, "in flight");
        async move { publisher.publish(event).await }
    });
    assert!(matches!(relay.next().await, Seen::Event(_)));

    relay.kick.send(()).unwrap();
    wait_state(&socket, |s| *s != SocketState::Ready).await;
    // Never replayed: the sent publication may have landed.
    assert_eq!(
        publication.await.unwrap(),
        Err(PublishError::Unconfirmed {
            message: "Socket connection interrupted".into(),
            refusal: None,
        })
    );

    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(
        matches!(relay.next().await, Seen::Auth(_)),
        "re-authenticates"
    );
    let Seen::Req(second_id, filters) = relay.next().await else {
        panic!("expected the REQ again");
    };
    assert_eq!(first_id, second_id);
    assert_eq!(filters, vec![json!({"kinds": [9]})]);
    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Resubscribed
    ));
    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Eose
    ));
}

#[tokio::test]
async fn publish_without_a_connection_is_not_sent() {
    let (identity, keys) = viewer();
    let socket = RelaySocket::connect(
        &tokio::runtime::Handle::current(),
        RelayOrigin::parse("ws://127.0.0.1:1").unwrap(),
        identity.clone(),
        Arc::new(Verifier::default()),
        options(),
    );
    let result = tokio::time::timeout(Duration::from_secs(3), socket.publish(note(&keys, "x")))
        .await
        .expect("publish must not wait past its deadline");
    assert!(
        matches!(result, Err(PublishError::NotSent(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn without_resubscribe_subscriptions_are_reported_lost() {
    let mut relay = FakeRelay::start(Behaviour::default()).await;
    let (identity, _) = viewer();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            resubscribe: false,
            ..options()
        },
    );
    let mut subscription = socket.subscribe(vec![filter(json!({"kinds": [9]}))]);
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));
    assert!(matches!(relay.next().await, Seen::Req(..)));
    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Eose
    ));

    relay.kick.send(()).unwrap();
    assert!(matches!(
        next_event(&mut subscription).await,
        SubscriptionEvent::Lost
    ));
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));
    // Nothing is re-sent on the new connection.
    assert!(relay.quiet(Duration::from_millis(500)).await);
}

#[tokio::test]
async fn presence_is_lossy_and_honours_the_cooldown() {
    let mut relay = FakeRelay::start(Behaviour::default()).await;
    let (identity, keys) = viewer();
    let cooldown = Cooldown::default();
    let socket = connect(
        &relay,
        &identity,
        SocketOptions {
            cooldown: cooldown.clone(),
            ..options()
        },
    );
    let publisher = socket.publisher();
    wait_state(&socket, |s| *s == SocketState::Ready).await;
    assert!(matches!(relay.next().await, Seen::Connected));
    assert!(matches!(relay.next().await, Seen::Auth(_)));
    assert_eq!(
        publisher.publish_presence(note(&keys, "online")).await,
        PresenceSend::Accepted
    );
    assert!(matches!(relay.next().await, Seen::Event(_)));
    // A refusal is unconfirmed, and a quota one pauses the shared cooldown.
    assert_eq!(
        publisher.publish_presence(note(&keys, "busy")).await,
        PresenceSend::Unconfirmed
    );
    assert!(matches!(relay.next().await, Seen::Event(_)));
    assert!(cooldown.remaining() > Duration::from_secs(3));
    assert!(matches!(
        publisher.publish_presence(note(&keys, "away")).await,
        PresenceSend::Cooling(wait) if wait > Duration::from_secs(3)
    ));
    assert!(relay.quiet(Duration::from_millis(200)).await);
}
