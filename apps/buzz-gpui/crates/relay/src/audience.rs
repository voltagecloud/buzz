//! Who a message is for (`audience.ts`).

use nostr::prelude::Event;

/// A message's signed `audience` tag, when it names agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Audience {
    /// Coordination between agents, which people see quietly and are not
    /// notified of.
    Agents,
}

/// The audience `event` signs. Any value other than `agents`, or none, is
/// for everyone.
pub fn audience(event: &Event) -> Option<Audience> {
    event
        .tags
        .iter()
        .any(|tag| matches!(tag.as_slice(), [name, value, ..] if name == "audience" && value == "agents"))
        .then_some(Audience::Agents)
}
