use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Kind, Tag};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::{keys, BOUND, CHANNEL};

pub(super) struct Relay {
    pub url: String,
    pub events: Arc<Mutex<Vec<Event>>>,
    pub lose_next_ack: Arc<AtomicBool>,
    subscriptions: watch::Receiver<BTreeMap<String, usize>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Relay {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let metadata = EventBuilder::new(Kind::Custom(39000), "")
            .tags([
                Tag::parse(["d", CHANNEL]).unwrap(),
                Tag::parse(["t", "stream"]).unwrap(),
            ])
            .sign_with_keys(&keys("11"))
            .unwrap();
        let listing = EventBuilder::new(Kind::Custom(30177), "{}")
            .tags([
                Tag::parse(["d", &keys("22").public_key().to_hex()]).unwrap(),
                Tag::parse(["runtime", "voltage.remote-workspace/sandbox"]).unwrap(),
            ])
            .sign_with_keys(&keys("33"))
            .unwrap();
        let members = EventBuilder::new(Kind::Custom(39002), "")
            .tags([
                Tag::parse(["d", CHANNEL]).unwrap(),
                Tag::public_key(keys("22").public_key()),
            ])
            .sign_with_keys(&keys("11"))
            .unwrap();
        let events = Arc::new(Mutex::new(vec![metadata, listing, members]));
        let cancel = CancellationToken::new();
        let lose_next_ack = Arc::new(AtomicBool::new(false));
        let (subscriptions, receiver) = watch::channel(BTreeMap::new());
        let task = {
            let events = events.clone();
            let cancel = cancel.clone();
            let lose_next_ack = lose_next_ack.clone();
            tokio::spawn(async move {
                let mut tasks = JoinSet::new();
                let (hub, _) = broadcast::channel(256);
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        Some(_) = tasks.join_next() => {},
                        socket = listener.accept() => {
                            let (socket, _) = socket.unwrap();
                            let events = events.clone(); let hub = hub.clone(); let subscriptions = subscriptions.clone();
                            let lose_next_ack = lose_next_ack.clone();
                            tasks.spawn(async move { let _ = connection(socket, events, hub, subscriptions, lose_next_ack).await; });
                        }
                    }
                }
                tasks.shutdown().await;
            })
        };
        Self {
            url,
            events,
            lose_next_ack,
            subscriptions: receiver,
            cancel,
            task,
        }
    }

    pub fn count(&self, id: &str) -> usize {
        self.subscriptions.borrow().get(id).copied().unwrap_or(0)
    }

    pub async fn wait_subscription(&self, id: &str) {
        self.wait_after(id, 0).await;
    }

    pub async fn wait_after(&self, id: &str, previous: usize) {
        let mut receiver = self.subscriptions.clone();
        tokio::time::timeout(
            BOUND,
            receiver.wait_for(|v| v.get(id).copied().unwrap_or(0) > previous),
        )
        .await
        .unwrap()
        .unwrap();
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        self.task.await.unwrap();
    }
}

fn matches(filter: &Value, event: &Event) -> bool {
    let contains = |name: &str, value: Value| {
        filter[name]
            .as_array()
            .is_none_or(|values| values.contains(&value))
    };
    contains("kinds", json!(event.kind.as_u16()))
        && contains("authors", json!(event.pubkey))
        && contains("ids", json!(event.id))
        && ["h", "d"].into_iter().all(|key| {
            filter[format!("#{key}")].as_array().is_none_or(|values| {
                event.tags.iter().any(|tag| {
                    tag.as_slice().first().map(String::as_str) == Some(key)
                        && tag
                            .as_slice()
                            .get(1)
                            .is_some_and(|value| values.contains(&json!(value)))
                })
            })
        })
        && filter["#p"].as_array().is_none_or(|values| {
            event.tags.iter().any(|tag| {
                let tag = tag.as_slice();
                tag.first().map(String::as_str) == Some("p")
                    && tag.get(1).is_some_and(|key| values.contains(&json!(key)))
            })
        })
        && filter["since"]
            .as_u64()
            .is_none_or(|since| event.created_at.as_secs() >= since)
        && filter["until"]
            .as_u64()
            .is_none_or(|until| event.created_at.as_secs() <= until)
}

async fn connection(
    mut socket: tokio::net::TcpStream,
    stored: Arc<Mutex<Vec<Event>>>,
    hub: broadcast::Sender<Event>,
    subscriptions: watch::Sender<BTreeMap<String, usize>>,
    lose_next_ack: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // The authority lookup reads NIP-11 from the same authenticated community
    // origin. Peek without consuming the WebSocket handshake.
    let mut prefix = [0_u8; 16_384];
    for _ in 0..64 {
        let n = tokio::time::timeout(std::time::Duration::from_secs(1), socket.peek(&mut prefix))
            .await??;
        let text = String::from_utf8_lossy(&prefix[..n]);
        if text.contains("\r\n\r\n") {
            if !text.to_ascii_lowercase().contains("sec-websocket-key:") {
                let body = json!({"self":keys("11").public_key()}).to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await?;
                return Ok(());
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let mut ws = tokio_tungstenite::accept_async(socket).await?;
    let frame = |value: Value| Message::Text(value.to_string().into());
    ws.send(frame(json!(["AUTH", "challenge"]))).await?;
    let mut events = hub.subscribe();
    let mut author = None;
    let mut filters = BTreeMap::<String, Value>::new();
    loop {
        tokio::select! {
            input = ws.next() => {
                let Some(Ok(Message::Text(text))) = input else { return Ok(()) };
                let input: Value = serde_json::from_str(&text)?;
                match input[0].as_str().unwrap() {
                    "AUTH" => {
                        let event: Event = serde_json::from_value(input[1].clone())?;
                        event.verify()?;
                        if event.pubkey == keys("22").public_key() {
                            let auth = event.tags.iter().find(|t| t.as_slice()[0] == "auth").unwrap();
                            assert_eq!(buzz_sdk::nip_oa::verify_auth_tag(&serde_json::to_string(auth.as_slice())?, &event.pubkey)?, keys("33").public_key());
                        }
                        author = Some(event.pubkey);
                        ws.send(frame(json!(["OK", event.id, true, ""]))).await?;
                    }
                    "REQ" => {
                        assert!(author.is_some());
                        let id = input[1].as_str().unwrap().to_string();
                        let filter = input[2].clone();
                        if matches!(id.as_str(), "mention-gateways" | "mention-gateway-profiles" | "mention-gateway-channel" | "mention-channel" | "mention-listing") {
                            assert_eq!(filter["consistency"], json!("strong"));
                        }
                        let allowed = |event: &Event| matches!(event.kind.as_u16(), 0 | 24210 | 30177 | 30181) || event.tags.iter().any(|tag| tag.as_slice() == ["h", CHANNEL] || tag.as_slice() == ["d", CHANNEL]);
                        let mut history: Vec<_> = stored.lock().unwrap().iter().filter(|e| matches(&filter,e) && allowed(e)).cloned().collect();
                        history.sort_by_key(|e| std::cmp::Reverse((e.created_at, e.id)));
                        history.truncate(filter["limit"].as_u64().unwrap_or(1000) as usize);
                        filters.insert(id.clone(), filter);
                        for event in history { ws.send(frame(json!(["EVENT", id, event]))).await?; }
                        ws.send(frame(json!(["EOSE", id]))).await?;
                        subscriptions.send_modify(|counts| { *counts.entry(id).or_default() += 1; });
                    }
                    "CLOSE" => {
                        filters.remove(input[1].as_str().unwrap());
                        ws.send(frame(json!(["CLOSED",input[1],""]))).await?;
                        subscriptions.send_modify(|counts| { *counts.entry(format!("closed:{}",input[1].as_str().unwrap())).or_default() += 1; });
                    }
                    "EVENT" => {
                        let event: Event = serde_json::from_value(input[1].clone())?;
                        let accepted = Some(event.pubkey) == author && event.verify().is_ok()
                            && (event.kind.as_u16() == 24210 || event.tags.iter().any(|tag| tag.as_slice() == ["h",CHANNEL]));
                        if accepted {
                            let mut history = stored.lock().unwrap();
                            if !history.iter().any(|e| e.id == event.id) { history.push(event.clone()); }
                            let _ = hub.send(event.clone());
                        }
                        if accepted && lose_next_ack.swap(false, Ordering::SeqCst) { return Ok(()); }
                        ws.send(frame(json!(["OK",event.id,accepted,""]))).await?;
                    }
                    _ => panic!("unexpected client protocol"),
                }
            }
            event = events.recv() => {
                let event = event?;
                if event.kind.as_u16() != 24210 && !event.tags.iter().any(|t| t.as_slice() == ["h", CHANNEL]) { continue; }
                for (id, filter) in &filters {
                    if event.kind.as_u16()!=24210 && filter.get("#h").is_none() {continue;}
                    if matches(filter, &event) { ws.send(frame(json!(["EVENT",id,event]))).await?; }
                }
            }
        }
    }
}
