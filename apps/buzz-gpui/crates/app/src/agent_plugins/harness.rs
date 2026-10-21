//! What the agent types share (buzz-app's `features/agent-harness`): the
//! Buzz base prompt they give their agents before their own instructions,
//! and reads of an agent's memory. The memory is encrypted to the owner,
//! so it is read back through the owner's session; no agent key is needed.
//! `base_prompt.md` is buzz-app's, copied unchanged.

use crate::agent_plugins::owner_relay::OwnerRelay;
use crate::buzz_mcp::Memory;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The Buzz base prompt every agent type gives its agents.
pub fn base_prompt() -> &'static str {
    include_str!("base_prompt.md").trim()
}

/// How long a shared read may take; each caller can stop waiting sooner.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The agent's memories, and whether some records could not be read.
type Read = Shared<BoxFuture<'static, Result<(Vec<Memory>, bool), String>>>;
/// Each running read by community and agent, with its id.
type Reads = HashMap<(usize, String), (u64, Read)>;

/// One owner read per community and agent at a time, which concurrent
/// turns and tool calls share. Cheap to clone.
#[derive(Clone, Default)]
pub struct MemoryReads {
    reads: Arc<Mutex<Reads>>,
    next: Arc<AtomicU64>,
}

/// The community a reader reads, by the reader's identity.
fn community(relay: &Arc<dyn OwnerRelay>) -> usize {
    Arc::as_ptr(relay) as *const () as usize
}

impl MemoryReads {
    fn reads(&self) -> std::sync::MutexGuard<'_, Reads> {
        self.reads.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The agent's memory, joining a read already running. Must run inside
    /// tokio.
    pub fn read(&self, relay: &Arc<dyn OwnerRelay>, agent: &str) -> Read {
        let key = (community(relay), agent.to_string());
        if let Some((_, read)) = self.reads().get(&key) {
            return read.clone();
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let started = relay.memory_listing(agent);
        let read: Read = async move {
            tokio::time::timeout(READ_TIMEOUT, started)
                .await
                .unwrap_or_else(|_| Err("Memory read timed out".into()))
        }
        .boxed()
        .shared();
        self.reads().insert(key.clone(), (id, read.clone()));
        let (this, done) = (self.clone(), read.clone());
        tokio::spawn(async move {
            let _ = done.await;
            let mut reads = this.reads();
            if reads.get(&key).is_some_and(|(current, _)| *current == id) {
                reads.remove(&key);
            }
        });
        read
    }

    /// After a memory write, a read that may have started before it is not
    /// shared with later callers.
    pub fn forget(&self, relay: &Arc<dyn OwnerRelay>, agent: &str) {
        self.reads().remove(&(community(relay), agent.to_string()));
    }
}

/// The `core` memory in a listing: its text, `Some(None)` when the agent
/// has none, or `None` when a partial listing may have missed it.
pub fn core_of(memories: &[Memory], partial: bool) -> Option<Option<String>> {
    match memories.iter().find(|memory| memory.slug == "core") {
        Some(core) => Some(Some(core.body.clone())),
        None if partial => None,
        None => Some(None),
    }
}

/// The `core` memory section of a prompt: its text, a prompt to create one
/// when it is `Some(None)`, or nothing when it is unread (`None`) or blank.
pub fn core_memory_section(memory: Option<Option<&str>>) -> String {
    let body = match memory {
        Some(None) => {
            "No core memory found. Use `mem_set` with slug `core` to create one (it will hold your identity, rules, and goals across sessions). Ask your user about yourself."
        }
        Some(Some(memory)) => memory.trim(),
        None => "",
    };
    if body.is_empty() {
        String::new()
    } else {
        format!("<core-memory>\n{body}\n</core-memory>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_plugins::owner_relay::ChannelInfo;
    use crate::buzz_mcp::BuzzEvent;
    use serde_json::Value;

    #[derive(Default)]
    struct Counting(Mutex<usize>);

    impl OwnerRelay for Counting {
        fn channel(&self, _: &str) -> Option<ChannelInfo> {
            None
        }
        fn read(&self, _: Vec<Value>) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn names(
            &self,
            _: Vec<String>,
            _: Vec<String>,
        ) -> BoxFuture<'static, HashMap<String, String>> {
            Box::pin(async { HashMap::new() })
        }
        fn memories(&self, _: &str) -> BoxFuture<'static, Result<Vec<Memory>, String>> {
            *self.0.lock().unwrap() += 1;
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(vec![Memory {
                    slug: "core".into(),
                    body: "I am Bee.".into(),
                    created_at: 1,
                }])
            })
        }
    }

    #[tokio::test]
    async fn concurrent_reads_share_one_until_a_write_forgets_it() {
        let counting = Arc::new(Counting::default());
        let relay: Arc<dyn OwnerRelay> = counting.clone();
        let reads = MemoryReads::default();
        let (a, b) = (reads.read(&relay, "agent"), reads.read(&relay, "agent"));
        assert_eq!(*counting.0.lock().unwrap(), 1);
        reads.forget(&relay, "agent");
        let c = reads.read(&relay, "agent");
        assert_eq!(*counting.0.lock().unwrap(), 2);
        let (a, _, _) = futures::join!(a, b, c);
        let (memories, partial) = a.unwrap();
        assert_eq!(core_of(&memories, partial), Some(Some("I am Bee.".into())));
        // A partial listing without `core` may have missed it.
        assert_eq!(core_of(&[], true), None);
        assert_eq!(core_of(&[], false), Some(None));
        // Once settled, the next read starts afresh.
        tokio::time::sleep(Duration::from_millis(5)).await;
        reads.read(&relay, "agent").await.unwrap();
        assert_eq!(*counting.0.lock().unwrap(), 3);
    }

    #[test]
    fn writes_the_core_memory_section() {
        assert_eq!(
            core_memory_section(Some(Some(" Be kind. "))),
            "<core-memory>\nBe kind.\n</core-memory>"
        );
        assert!(core_memory_section(Some(None)).contains("No core memory found."));
        assert_eq!(core_memory_section(None), "");
        assert_eq!(core_memory_section(Some(Some("  "))), "");
    }
}
