//! The rendered message order and the send clock (`message-order.ts`,
//! §5b.2, §9.2 #19-20).

use super::MessageRow;
use nostr::prelude::Event;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

/// Largest lead over the local clock that other authors' timestamps may
/// impose on our next send.
const MAX_LEAD_MS: u64 = 5_000;
/// Largest lead our own send chain may keep. Beyond it (a clock rollback)
/// the chain restarts at the clock. Well inside the relay's ±900 s.
const MAX_OWN_LEAD_MS: u64 = 60_000;

/// `created_at` in milliseconds plus the sub-second `["ms","<0..999>"]`
/// offset. Only a canonical 0-999 value counts; anything else is 0.
pub fn event_ms(event: &Event) -> u64 {
    let offset = event
        .tags
        .iter()
        .find(|tag| tag.kind() == "ms")
        .and_then(|tag| tag.content())
        .filter(|value| {
            let bytes = value.as_bytes();
            (1..=3).contains(&bytes.len())
                && bytes.iter().all(u8::is_ascii_digit)
                && (bytes[0] != b'0' || bytes.len() == 1)
        })
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    event.created_at.as_secs() * 1000 + offset
}

/// The one rendered order: effective milliseconds, then id. Channel and
/// thread views both use it, so they never disagree.
pub fn compare_messages(a: &MessageRow, b: &MessageRow) -> Ordering {
    a.created_at_ms
        .cmp(&b.created_at_ms)
        .then_with(|| a.id.cmp(&b.id))
}

/// Per-session send clock with per-channel high-water marks. One relay
/// session owns one clock, so channel ids never mix across communities or
/// viewers. Shared between the outbox and projections, hence the lock.
#[derive(Debug, Default)]
pub struct MessageClock {
    state: Mutex<ClockState>,
}

#[derive(Debug, Default)]
struct ClockState {
    latest: HashMap<String, u64>,
    sent: HashMap<String, u64>,
}

impl MessageClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records rendered evidence so the next send sorts after it.
    pub fn observe(&self, channel: &str, ms: u64) {
        let mut state = self.state.lock().expect("message clock lock");
        observe(&mut state, channel, ms);
    }

    /// A send time after everything known in the channel. Other authors'
    /// evidence may push it at most 5 s past `now_ms`; our own sends stay
    /// strictly increasing (up to a 60 s lead), so a burst never rewinds or
    /// ties even when evidence is far in the future.
    pub fn next(&self, channel: &str, now_ms: u64) -> u64 {
        let mut state = self.state.lock().expect("message clock lock");
        let latest = state.latest.get(channel).copied().unwrap_or(0);
        let evidence = (latest + 1).min(now_ms + MAX_LEAD_MS);
        let own = state.sent.get(channel).copied().unwrap_or(0) + 1;
        let own = if own.saturating_sub(now_ms) > MAX_OWN_LEAD_MS {
            0
        } else {
            own
        };
        let ms = now_ms.max(evidence).max(own);
        state.sent.insert(channel.to_owned(), ms);
        observe(&mut state, channel, ms);
        ms
    }
}

fn observe(state: &mut ClockState, channel: &str, ms: u64) {
    match state.latest.get_mut(channel) {
        Some(latest) => *latest = (*latest).max(ms),
        None => {
            state.latest.insert(channel.to_owned(), ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_other_authors_lead_without_rewinding_or_tying_local_sends() {
        let now = 2_000_000_000_000;
        let clock = MessageClock::new();
        clock.observe("near", now + 4999);
        let near: Vec<u64> = (0..3).map(|_| clock.next("near", now)).collect();
        assert_eq!(near, [now + 5000, now + 5001, now + 5002]);

        // A skewed peer far in the future.
        clock.observe("far", now + 60_000);
        let far: Vec<u64> = (0..3).map(|_| clock.next("far", now)).collect();
        assert_eq!(far, [now + 5000, now + 5001, now + 5002]);

        // Another channel's allocations do not leak in.
        assert_eq!(clock.next("elsewhere", now), now);
    }

    #[test]
    fn restarts_the_local_chain_at_the_clock_after_a_large_rollback() {
        let now = 2_050_000_000_000;
        let clock = MessageClock::new();
        assert_eq!(clock.next("rollback", now), now);
        assert_eq!(clock.next("rollback", now - 120_000), now - 115_000);
        assert_eq!(clock.next("rollback", now - 120_000), now - 114_999);
    }

    #[test]
    fn bursts_after_newer_evidence_keep_their_order() {
        for offset in [700, 998] {
            let now = 2_100_000_000_000;
            let clock = MessageClock::new();
            clock.observe("burst", now + offset);
            let sent: Vec<u64> = (0..3).map(|_| clock.next("burst", now)).collect();
            assert_eq!(sent, [now + offset + 1, now + offset + 2, now + offset + 3]);
        }
    }
}
