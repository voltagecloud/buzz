//! Fresh shared-task admission. The manager owns authorization and its durable
//! budget. A retry uses a new transport request ID, so its response cache cannot
//! preserve an approval after revocation; the task receipt remains idempotent.

use buzz_session_control::{build_request, open_reply, Op, Request, Status, KIND};
use buzz_ws_client::{NostrWsConnection, RelayMessage};
use nostr::{Event, EventBuilder, Keys, Kind, PublicKey, Tag, Timestamp};
use serde_json::json;
use std::time::Duration;

pub(crate) mod completion;
mod notices;

#[cfg(test)]
mod tests;

/// Admission transport failure. It propagates to the task owner for recovery.
#[derive(Debug, thiserror::Error)]
#[error("shared task admission unavailable; retry or recover this task")]
pub struct Error;

/// A verified manager admission, or a permanent refusal safe to show the user.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Allowed(PublicKey),
    Refused(String),
}

/// Agent credentials used only for encrypted requests and signed channel replies.
/// Deliberately has no Debug implementation.
#[derive(Clone)]
pub struct Client {
    keys: Keys,
    auth: String,
    relay: String,
    manager: PublicKey,
    scope: buzz_sdk::sharing::task::Scope,
    rest: crate::relay::RestClient,
    outbox: std::sync::Arc<tokio::sync::Mutex<notices::Outbox>>,
}

impl Client {
    pub(crate) fn from_config(
        config: &crate::config::Config,
        rest: &crate::relay::RestClient,
    ) -> Result<Option<Self>, Error> {
        let Some(manager) = config.sharing_manager else {
            return Ok(None);
        };
        let auth = rest.auth_tag_json.clone().ok_or(Error)?;
        let crate::config::BoundSession::Thread {
            channel_id,
            thread_root,
        } = config.bound_session.as_ref().ok_or(Error)?
        else {
            return Err(Error);
        };
        let scope = buzz_sdk::sharing::task::Scope {
            channel: *channel_id,
            root: nostr::EventId::from_hex(thread_root).map_err(|_| Error)?,
        };
        let directory = std::env::current_dir()
            .map_err(|_| Error)?
            .join(".buzz/shared-failures")
            .join(config.keys.public_key().to_hex())
            .join(scope.root.to_hex());
        Ok(Some(Self {
            keys: config.keys.clone(),
            auth,
            relay: config.relay_url.clone(),
            manager,
            scope,
            rest: rest.clone(),
            outbox: std::sync::Arc::new(tokio::sync::Mutex::new(notices::Outbox::new(directory))),
        }))
    }

    pub(crate) fn accepts(&self, event: &Event) -> bool {
        buzz_sdk::sharing::task::scope(event, self.keys.public_key(), Timestamp::now().as_secs())
            .ok()
            == Some(self.scope)
    }

    /// Return a permanent refusal separately from a retryable transport failure.
    pub(crate) async fn authorize(&self, event: &Event) -> Result<Admission, Error> {
        if !self.accepts(event) {
            return Ok(Admission::Refused(
                "task is outside this bound conversation".into(),
            ));
        }
        tokio::time::timeout(Duration::from_secs(50), self.exchange(event))
            .await
            .map_err(|_| Error)?
    }

    async fn exchange(&self, trigger: &Event) -> Result<Admission, Error> {
        let auth = buzz_sdk::nip_oa::parse_auth_tag(&self.auth).map_err(|_| Error)?;
        let request = Request::new(
            Op::Admit,
            uuid::Uuid::new_v4().to_string(),
            json!({"relay_url":self.relay,"trigger":trigger}),
        );
        let event = build_request(
            &self.keys,
            &self.manager,
            Some(&self.auth),
            &request,
            Timestamp::now(),
        )
        .map_err(|_| Error)?;
        let id = event.id;
        let mut connection =
            NostrWsConnection::connect_authenticated(&self.relay, &self.keys, Some(&auth))
                .await
                .map_err(|_| Error)?;
        connection.send_raw(&json!(["REQ","shared-reply",{"kinds":[KIND],"authors":[self.manager],"#p":[self.keys.public_key()],"#e":[id],"limit":0}])).await.map_err(|_|Error)?;
        let mut ready = false;
        for _ in 0..8 {
            match connection
                .next_event(Duration::from_secs(10))
                .await
                .map_err(|_| Error)?
            {
                RelayMessage::Eose { subscription_id } if subscription_id == "shared-reply" => {
                    ready = true;
                    break;
                }
                RelayMessage::Closed { .. } | RelayMessage::Auth { .. } => return Err(Error),
                _ => {}
            }
        }
        if !ready {
            return Err(Error);
        }
        if !connection
            .send_event(event)
            .await
            .map_err(|_| Error)?
            .accepted
        {
            return Err(Error);
        }
        for _ in 0..32 {
            match connection
                .next_event(Duration::from_secs(45))
                .await
                .map_err(|_| Error)?
            {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } if subscription_id == "shared-reply" => {
                    let reply =
                        open_reply(&event, &self.keys, &self.manager, &id).map_err(|_| Error)?;
                    if reply.request_id != request.request_id {
                        return Err(Error);
                    }
                    if reply.status != Status::Done {
                        continue;
                    }
                    let response = reply.response.ok_or(Error)?;
                    if response["ok"] == true {
                        return Ok(Admission::Allowed(admitted_author(&response, trigger)?));
                    }
                    if response["code"] == "shared_task_retry" {
                        return Err(Error);
                    }
                    let reason = response["error"]
                        .as_str()
                        .unwrap_or("task permission was refused");
                    return Ok(Admission::Refused(reason.chars().take(500).collect()));
                }
                RelayMessage::Closed { .. } | RelayMessage::Auth { .. } => return Err(Error),
                _ => {}
            }
        }
        Err(Error)
    }

    pub(crate) async fn refuse(&self, trigger: &Event, reason: &str) -> Result<(), Error> {
        if !self.accepts(trigger) {
            return Err(Error);
        }
        {
            let scope = buzz_sdk::sharing::task::scope(
                trigger,
                self.keys.public_key(),
                Timestamp::now().as_secs(),
            )
            .map_err(|_| Error)?;
            let tags = [
                vec!["h".into(), scope.channel.to_string()],
                vec!["e".into(), scope.root.to_hex(), "".into(), "root".into()],
                vec!["e".into(), trigger.id.to_hex(), "".into(), "reply".into()],
            ]
            .into_iter()
            .map(Tag::parse)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error)?;
            // No p tag: a refusal must not wake the handing-off agent in a loop.
            let notice = EventBuilder::new(
                Kind::Custom(9),
                format!("Could not accept this shared task: {reason}"),
            )
            .tags(tags)
            .sign_with_keys(&self.keys)
            .map_err(|_| Error)?;
            self.outbox.lock().await.reserve(trigger.id, notice)?;
        }
        self.flush_notice().await
    }

    pub(crate) async fn flush_notice(&self) -> Result<(), Error> {
        let outbox = self.outbox.lock().await;
        let now = Timestamp::now().as_secs();
        let Some((path, mut notice)) = outbox.next(now)? else {
            return Ok(());
        };
        if notice.event.pubkey != self.keys.public_key()
            || notice.event.verify().is_err()
            || !notice
                .event
                .tags
                .iter()
                .any(|t| t.as_slice() == ["h", &self.scope.channel.to_string()])
            || !notice
                .event
                .tags
                .iter()
                .any(|t| t.as_slice() == ["e", &self.scope.root.to_hex(), "", "root"])
            || (!notice.completion
                && notice
                    .event
                    .tags
                    .iter()
                    .any(|t| t.as_slice().first().map(String::as_str) == Some("p")))
            || (notice.completion && !self.valid_completion(&notice.event))
        {
            return Err(Error);
        }
        outbox.attempt(&path, &mut notice, now)?;
        if notice.completion {
            match self.completion_needed(&notice.event).await {
                Ok(false) => return outbox.complete(&path),
                Ok(true) => {}
                Err(_) => {
                    tracing::warn!(
                        code = "handoff_completion_lookup_retry",
                        category = "transport",
                        "handoff completion retained until return status can be established"
                    );
                    return Ok(());
                }
            }
        }
        let delivered = tokio::time::timeout(Duration::from_secs(10), async {
            let auth = buzz_sdk::nip_oa::parse_auth_tag(&self.auth).map_err(|_| Error)?;
            let mut connection =
                NostrWsConnection::connect_authenticated(&self.relay, &self.keys, Some(&auth))
                    .await
                    .map_err(|_| Error)?;
            if connection
                .send_event(notice.event)
                .await
                .map_err(|_| Error)?
                .accepted
            {
                Ok(())
            } else {
                Err(Error)
            }
        })
        .await;
        if matches!(delivered, Ok(Ok(()))) {
            outbox.complete(&path)?;
        } else {
            tracing::warn!("shared task refusal retained for bounded delivery retry");
        }
        Ok(())
    }

    pub(crate) fn start_notices(self: std::sync::Arc<Self>) -> NoticeWorker {
        NoticeWorker(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if self.flush_notice().await.is_err() {
                    tracing::error!(
                        "shared refusal journal needs repair; persisted notices retained"
                    );
                    return;
                }
            }
        }))
    }
}

// Called only after authenticating the manager's encrypted, request-bound reply.
// Root tasks adopt the authorizing human; handoffs retain the sending agent.
fn admitted_author(response: &serde_json::Value, trigger: &Event) -> Result<PublicKey, Error> {
    if buzz_sdk::sharing::task::origin(trigger)
        .map_err(|_| Error)?
        .is_some()
    {
        return Ok(trigger.pubkey);
    }
    let Some(value) = response.get("human") else {
        return Ok(trigger.pubkey);
    };
    let text = value.as_str().ok_or(Error)?;
    let key = PublicKey::from_hex(text).map_err(|_| Error)?;
    if key.to_hex() != text {
        return Err(Error);
    }
    Ok(key)
}

pub(crate) struct NoticeWorker(tokio::task::JoinHandle<()>);
impl Drop for NoticeWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}
