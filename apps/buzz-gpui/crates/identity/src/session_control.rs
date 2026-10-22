//! Kind 24210 Sandbox session control as the owner
//! (`buzz-session-control`): the `enroll` request a Sandbox manager takes
//! only from the owner key itself, and the manager's replies to it. Built
//! with the `session-control` feature.

use super::Identity;
use buzz_session_control::{self as control, Op, Reply, Request, WireError};
use nostr::prelude::{Event, EventId, PublicKey};
use nostr_wire::JsonUtil as _;

impl Identity {
    /// The owner's signed `enroll` request carrying `profile` to `manager`.
    /// Its content is NIP-44 ciphertext to the manager; only `p` and
    /// `expiration` are in the clear.
    pub fn enroll_request(
        &self,
        manager: &PublicKey,
        request_id: &str,
        profile: serde_json::Value,
        created_at: u64,
    ) -> Result<Event, WireError> {
        let payload = serde_json::json!({ "profile": profile });
        let request = Request::new(Op::Enroll, request_id, payload);
        let event = control::build_request(
            &self.wire_keys()?,
            &wire_key(manager)?,
            None,
            &request,
            nostr_wire::Timestamp::from(created_at),
        )?;
        Event::from_json(event.as_json()).map_err(|_| WireError::Sign)
    }

    /// Opens `manager`'s reply to our request event `request`.
    pub fn open_session_reply(
        &self,
        event: &Event,
        manager: &PublicKey,
        request: &EventId,
    ) -> Result<Reply, WireError> {
        let event = nostr_wire::Event::from_json(event.as_json())
            .map_err(|_| WireError::MalformedEnvelope)?;
        let request = nostr_wire::EventId::from_slice(request.as_bytes())
            .map_err(|_| WireError::WrongRequest)?;
        control::open_reply(&event, &self.wire_keys()?, &wire_key(manager)?, &request)
    }

    /// Our key, as `buzz-session-control`'s nostr version takes it.
    fn wire_keys(&self) -> Result<nostr_wire::Keys, WireError> {
        let secret = nostr_wire::SecretKey::from_slice(self.secret_bytes().as_slice())
            .map_err(|_| WireError::Sign)?;
        Ok(nostr_wire::Keys::new(secret))
    }
}

fn wire_key(key: &PublicKey) -> Result<nostr_wire::PublicKey, WireError> {
    nostr_wire::PublicKey::from_slice(&key.to_bytes()).map_err(|_| WireError::WrongRecipient)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_wire::Keys;

    fn manager() -> Keys {
        Keys::parse(&"07".repeat(32)).unwrap()
    }

    fn manager_key(keys: &Keys) -> PublicKey {
        PublicKey::from_hex(&keys.public_key().to_hex()).unwrap()
    }

    fn wire(event: &Event) -> nostr_wire::Event {
        nostr_wire::Event::from_json(event.as_json()).unwrap()
    }

    fn app(event: &nostr_wire::Event) -> Event {
        Event::from_json(event.as_json()).unwrap()
    }

    #[test]
    fn the_manager_opens_an_enroll_request_signed_by_the_owner() {
        let (owner, manager) = (Identity::ephemeral(), manager());
        let profile = serde_json::json!({"github": {"token": "ghp_fixture"}});
        let event = owner
            .enroll_request(
                &manager_key(&manager),
                "enroll-1",
                profile.clone(),
                1_700_000_000,
            )
            .unwrap();
        assert_eq!(event.pubkey, owner.public_key());
        let tags: Vec<_> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice()[0].clone())
            .collect();
        assert_eq!(tags, ["p", "expiration"]);
        assert!(!event.as_json().contains("ghp_fixture"));

        let opened = control::open_request(&wire(&event), &manager).unwrap();
        assert_eq!(opened.signer.to_hex(), owner.pubkey_hex());
        assert_eq!(opened.auth_tag, None);
        assert_eq!(opened.request.op, Op::Enroll);
        assert_eq!(opened.request.request_id, "enroll-1");
        assert_eq!(
            opened.request.payload,
            serde_json::json!({ "profile": profile })
        );
    }

    #[test]
    fn only_the_managers_reply_to_this_request_opens() {
        let (owner, manager) = (Identity::ephemeral(), manager());
        let request = owner
            .enroll_request(&manager_key(&manager), "enroll-1", serde_json::json!({}), 0)
            .unwrap();
        let reply = |keys: &Keys, to: &Event| {
            let done = Reply::done("enroll-1", serde_json::json!({"ok": true}));
            let requester = nostr_wire::PublicKey::from_hex(&owner.pubkey_hex()).unwrap();
            app(&control::build_reply(keys, &requester, &wire(to).id, &done, 0.into()).unwrap())
        };
        let manager_pubkey = manager_key(&manager);
        let opened = owner
            .open_session_reply(&reply(&manager, &request), &manager_pubkey, &request.id)
            .unwrap();
        assert_eq!(opened.response, Some(serde_json::json!({"ok": true})));

        let stranger = Keys::parse(&"09".repeat(32)).unwrap();
        let forged = reply(&stranger, &request);
        assert_eq!(
            owner.open_session_reply(&forged, &manager_pubkey, &request.id),
            Err(WireError::WrongSender)
        );
        let other = owner
            .enroll_request(&manager_pubkey, "enroll-2", serde_json::json!({}), 0)
            .unwrap();
        assert_eq!(
            owner.open_session_reply(&reply(&manager, &other), &manager_pubkey, &request.id),
            Err(WireError::WrongRequest)
        );
    }
}
