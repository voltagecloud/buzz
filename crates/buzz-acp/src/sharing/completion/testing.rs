//! Shared fixture for tests that drive the real queue-disposition handlers.
use super::*;
use nostr::{EventBuilder, Keys, Kind};

pub(crate) struct Handoff {
    pub batch: crate::queue::FlushBatch,
    pub tasks: TaskTracker,
    client: Arc<Client>,
}

impl Handoff {
    pub async fn prepare(acp: &mut crate::acp::AcpClient, directory: &std::path::Path) -> Self {
        let owner = Keys::generate();
        let sender = Keys::generate();
        let worker = Keys::generate();
        let channel = uuid::Uuid::new_v4();
        let root = EventBuilder::new(Kind::Custom(9), "task")
            .tags([
                Tag::parse(["h", &channel.to_string()]).unwrap(),
                Tag::public_key(sender.public_key()),
            ])
            .sign_with_keys(&owner)
            .unwrap();
        let event = task::handoff(
            &root,
            sender.public_key(),
            worker.public_key(),
            "work",
            Timestamp::now().as_secs(),
        )
        .unwrap()
        .sign_with_keys(&sender)
        .unwrap();
        let client = Arc::new(Client {
            keys: worker.clone(),
            auth: String::new(),
            relay: "ws://127.0.0.1:1".into(),
            manager: owner.public_key(),
            scope: task::Scope {
                channel,
                root: root.id,
            },
            rest: crate::relay::RestClient {
                http: reqwest::Client::new(),
                base_url: "http://127.0.0.1:1".into(),
                keys: worker,
                auth_tag_json: None,
            },
            outbox: Arc::new(tokio::sync::Mutex::new(super::super::notices::Outbox::new(
                directory.to_owned(),
            ))),
        });
        let batch = crate::queue::FlushBatch {
            channel_id: channel,
            scope: crate::scope::SessionScope::Thread {
                channel_id: channel,
                root_event_id: root.id.to_hex(),
            },
            events: vec![crate::queue::BatchEvent {
                event,
                prompt_tag: String::new(),
                received_at: std::time::Instant::now(),
                edit: None,
            }],
            cancelled_events: Vec::new(),
            cancel_reason: None,
        };
        let tasks = TaskTracker::new();
        let guard = Guard::begin(acp, Some(client.clone()), Some(&batch), tasks.clone()).unwrap();
        guard.reserve().await.unwrap();
        Self {
            batch,
            tasks,
            client,
        }
    }

    pub async fn resume_with_late_queue_marker(&self, acp: &mut crate::acp::AcpClient) {
        let guard = Guard::begin(
            acp,
            Some(self.client.clone()),
            Some(&self.batch),
            self.tasks.clone(),
        )
        .unwrap();
        guard.reserve().await.unwrap();
        // The prior result's async transition may run after the next dispatch.
        self.client
            .outbox
            .lock()
            .await
            .mark_queued(&[self.batch.events[0].event.id]);
    }

    pub fn recover_abandoned(
        &self,
        queue: &crate::queue::EventQueue,
        pool: &crate::pool::AgentPool,
    ) {
        self.client.recover_abandoned(queue, pool);
    }

    pub async fn assert_terminal(&self, terminal: bool) {
        self.tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(5), self.tasks.wait())
            .await
            .unwrap();
        let next = self.client.outbox.lock().await.next(u64::MAX).unwrap();
        assert_eq!(
            next.is_some(),
            terminal,
            "only terminal batches release their live journal owner"
        );
        if let Some((_, record)) = next {
            assert!(record.phase == super::super::notices::Phase::Ready);
            assert_eq!(record.trigger, self.batch.events[0].event.id);
            assert_eq!(
                record.attempts, 0,
                "unavailable status lookup cannot spend publication eligibility"
            );
        }
    }
}
