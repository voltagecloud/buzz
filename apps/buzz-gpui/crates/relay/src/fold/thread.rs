//! NIP-10 thread references (`thread-reference.ts`, §5c.1). This matches
//! relay ingest (`buzz:crates/buzz-core/src/nip10.rs`).

use super::hex32;
use nostr::prelude::{Event, EventId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadReference {
    /// The canonical thread root to open.
    pub root_id: EventId,
    /// The immediate reply target.
    pub parent_id: EventId,
}

/// Only marked `["e", <64 hex>, <relay>, root|reply]` tags count, and the
/// last valid one of each marker wins. A lone `root` is not a reply, so it
/// gives `None`, as does a top-level event.
pub fn thread_reference(event: &Event) -> Option<ThreadReference> {
    let mut root = None;
    let mut reply = None;
    for tag in event.tags.iter() {
        let [name, value, _, marker, ..] = tag.as_slice() else {
            continue;
        };
        let Some(id) = (name == "e").then(|| hex32(value, true)).flatten() else {
            continue;
        };
        match marker.as_str() {
            "root" => root = Some(EventId::from_byte_array(id)),
            "reply" => reply = Some(EventId::from_byte_array(id)),
            _ => {}
        }
    }
    let parent_id = reply?;
    Some(ThreadReference {
        root_id: root.unwrap_or(parent_id),
        parent_id,
    })
}
