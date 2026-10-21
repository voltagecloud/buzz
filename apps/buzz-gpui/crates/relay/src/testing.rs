//! Signed fixtures for unit tests.

use crate::event::{Verified, Verifier};
use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

pub const T0: u64 = 1_700_000_000;

pub const CHANNEL: &str = "00000000-0000-4000-8000-000000000001";

/// A relay key, a viewer key and a verifier.
pub struct Signer {
    pub relay: Keys,
    pub viewer: Keys,
    verifier: Verifier,
}

impl Signer {
    pub fn new() -> Self {
        Self {
            relay: Keys::generate(),
            viewer: Keys::generate(),
            verifier: Verifier::new(4096),
        }
    }

    pub fn relay_hex(&self) -> String {
        self.relay.public_key().to_hex()
    }

    pub fn viewer_hex(&self) -> String {
        self.viewer.public_key().to_hex()
    }

    pub fn sign(
        &self,
        keys: &Keys,
        kind: u16,
        at: u64,
        content: &str,
        tags: &[&[&str]],
    ) -> Verified {
        let tags = tags
            .iter()
            .map(|tag| Tag::parse(tag.iter().map(|s| s.to_string())).unwrap());
        let event = EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(keys)
            .unwrap();
        self.verifier.verify(event).unwrap()
    }

    /// Signed by the relay, with empty content.
    pub fn relay(&self, kind: u16, at: u64, tags: &[&[&str]]) -> Verified {
        self.sign(&self.relay, kind, at, "", tags)
    }
}

/// A distinct valid pubkey for each `i` (1 and up).
pub fn key(i: u8) -> String {
    let secret = nostr::prelude::SecretKey::from_slice(&[i.max(1); 32]).unwrap();
    Keys::new(secret).public_key().to_hex()
}
