//! Fakes for the four external boundaries: the terminal, the relay, the
//! Sandbox manager's transport, and local tools. The manager fake opens the
//! real encrypted requests with the manager key and answers with real
//! signed replies, so the wire path under test is the production one.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use buzz_session_control::{build_reply, open_request, Op, Reply};
use nostr::{Event, Keys, Timestamp};
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use super::super::io::Prompter;
use super::super::manager::ControlTransport;
use super::super::profile::{self, GitHubIdentity};
use super::super::relay::{Claimed, JoinPolicy, Relay, Signer};
use super::super::tools::{SigningKey, Tools};
use crate::error::CliError;

/// One scripted answer.
pub enum Answer {
    Text(&'static str),
    Secret(String),
}

/// Scripted terminal; records everything it would have shown or asked.
pub struct ScriptPrompter {
    pub interactive: bool,
    pub answers: VecDeque<Answer>,
    pub transcript: Vec<String>,
}

impl ScriptPrompter {
    pub fn quiet() -> Self {
        Self {
            interactive: false,
            answers: VecDeque::new(),
            transcript: Vec::new(),
        }
    }

    pub fn scripted(answers: Vec<Answer>) -> Self {
        Self {
            interactive: true,
            answers: answers.into(),
            transcript: Vec::new(),
        }
    }
}

impl Prompter for &mut ScriptPrompter {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn say(&mut self, text: &str) {
        self.transcript.push(text.to_owned());
    }

    fn ask(
        &mut self,
        question: &str,
        default: Option<&str>,
        hint: &str,
    ) -> Result<String, CliError> {
        self.transcript.push(format!("? {question}"));
        if !self.interactive {
            return default
                .map(str::to_owned)
                .ok_or_else(|| CliError::Usage(format!("{}: {hint}", question.trim())));
        }
        match self.answers.pop_front() {
            Some(Answer::Text("")) => Ok(default.unwrap_or_default().to_owned()),
            Some(Answer::Text(text)) => Ok(text.to_owned()),
            Some(Answer::Secret(_)) => panic!("asked {question:?} but the script has a secret"),
            None => panic!("no scripted answer for {question:?}"),
        }
    }

    fn secret(&mut self, question: &str, hint: &str) -> Result<Zeroizing<String>, CliError> {
        self.transcript.push(format!("? {question}"));
        if !self.interactive {
            return Err(CliError::Usage(format!("{}: {hint}", question.trim())));
        }
        match self.answers.pop_front() {
            Some(Answer::Secret(text)) => Ok(Zeroizing::new(text)),
            Some(Answer::Text(_)) => panic!("asked secret {question:?} but the script has text"),
            None => panic!("no scripted secret for {question:?}"),
        }
    }
}

/// An in-memory community.
#[derive(Default)]
pub struct FakeRelay {
    pub inner: Mutex<RelayState>,
}

#[derive(Default)]
pub struct RelayState {
    pub members: HashSet<String>,
    pub invites: HashSet<String>,
    pub policy: Option<JoinPolicy>,
    pub accepted_policy: bool,
    pub events: Vec<Value>,
    pub published: usize,
    /// The next publish of this kind fails once.
    pub fail_kind: Option<u64>,
}

fn tag_values<'a>(event: &'a Value, name: &str) -> Vec<&'a str> {
    event["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tag| tag[0] == name)
        .filter_map(|tag| tag[1].as_str())
        .collect()
}

fn matches(filter: &Value, event: &Value) -> bool {
    let within = |key: &str, value: &Value| {
        filter
            .get(key)
            .and_then(Value::as_array)
            .is_none_or(|allowed| allowed.contains(value))
    };
    let tagged = |key: &str, tag: &str| {
        filter
            .get(key)
            .and_then(Value::as_array)
            .is_none_or(|allowed| {
                tag_values(event, tag)
                    .iter()
                    .any(|value| allowed.iter().any(|allowed| allowed == value))
            })
    };
    within("kinds", &event["kind"])
        && within("ids", &event["id"])
        && within("authors", &event["pubkey"])
        && tagged("#h", "h")
        && tagged("#d", "d")
        && tagged("#p", "p")
}

impl FakeRelay {
    pub fn with_invite(code: &str) -> Self {
        let relay = Self::default();
        relay.state().invites.insert(code.to_owned());
        relay
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, RelayState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn of_kind(&self, kind: u64) -> Vec<Value> {
        self.state()
            .events
            .iter()
            .filter(|event| event["kind"] == kind)
            .cloned()
            .collect()
    }
}

impl Relay for &FakeRelay {
    async fn is_member(&self, keys: &Keys) -> Result<bool, CliError> {
        Ok(self.state().members.contains(&keys.public_key().to_hex()))
    }

    async fn join_policy(&self) -> Result<Option<JoinPolicy>, CliError> {
        Ok(self.state().policy.clone())
    }

    async fn accept_policy(
        &self,
        _keys: &Keys,
        code: &str,
        policy: &JoinPolicy,
        _age: bool,
    ) -> Result<String, CliError> {
        let mut state = self.state();
        assert_eq!(state.policy.as_ref(), Some(policy));
        state.accepted_policy = true;
        Ok(format!("receipt-{code}"))
    }

    async fn claim(
        &self,
        keys: &Keys,
        code: &str,
        receipt: Option<&str>,
    ) -> Result<Claimed, CliError> {
        let mut state = self.state();
        if state.policy.is_some() && receipt != Some(&format!("receipt-{code}")) {
            return Err(CliError::Auth("join_policy_required".into()));
        }
        if !state.invites.contains(code) {
            return Err(CliError::Auth(
                "This invite code is not valid for this community.".into(),
            ));
        }
        Ok(if state.members.insert(keys.public_key().to_hex()) {
            Claimed::Joined
        } else {
            Claimed::AlreadyMember
        })
    }

    async fn query(&self, keys: &Keys, filter: Value) -> Result<Vec<Value>, CliError> {
        let state = self.state();
        if !state.members.contains(&keys.public_key().to_hex()) {
            return Err(CliError::Relay {
                status: 403,
                body: "restricted".into(),
            });
        }
        Ok(state
            .events
            .iter()
            .filter(|event| matches(&filter, event))
            .cloned()
            .collect())
    }

    async fn publish(&self, signer: &Signer, event: Event) -> Result<(), CliError> {
        assert!(event.verify().is_ok(), "the relay only takes valid events");
        assert_eq!(
            event.pubkey,
            signer.keys.public_key(),
            "NIP-98 signer is the author"
        );
        let mut state = self.state();
        let author = event.pubkey.to_hex();
        // Admission: a member, or an agent its member owner attests.
        let admitted = state.members.contains(&author)
            || signer.auth_tag.as_deref().is_some_and(|tag| {
                buzz_sdk::nip_oa::verify_auth_tag(tag, &event.pubkey).is_ok()
                    && state
                        .members
                        .contains(&serde_json::from_str::<Vec<String>>(tag).unwrap()[1])
            });
        if !admitted {
            return Err(CliError::Relay {
                status: 403,
                body: "not a member".into(),
            });
        }
        let value = serde_json::to_value(&event).unwrap();
        let kind = value["kind"].as_u64().unwrap();
        if state.fail_kind == Some(kind) {
            state.fail_kind = None;
            return Err(CliError::Relay {
                status: 503,
                body: "unavailable".into(),
            });
        }
        state.published += 1;
        if kind == 9007 {
            let id = tag_values(&value, "h")[0].to_owned();
            let name = tag_values(&value, "name")[0].to_owned();
            state
                .events
                .push(json!({"kind": 39000, "pubkey": "relay", "created_at": 0,
                "tags": [["d", id], ["name", name]], "content": ""}));
            state
                .events
                .push(json!({"kind": 39002, "pubkey": "relay", "created_at": 0,
                "tags": [["d", id], ["p", author]], "content": ""}));
        }
        state.events.push(value);
        Ok(())
    }
}

/// The Sandbox manager, reached through a fake transport.
pub struct FakeManager {
    pub keys: Keys,
    pub inner: Mutex<ManagerState>,
}

#[derive(Default)]
pub struct ManagerState {
    /// Accept every owner as a configured developer.
    pub any_developer: bool,
    pub developers: HashSet<String>,
    pub profile: Option<Value>,
    pub versions: u32,
    pub paseo: Option<Value>,
    pub ops: Vec<Op>,
    /// Agents whose next deploy is refused, with the refusal code.
    pub failing_deploys: BTreeMap<String, String>,
    /// The code the next enroll is refused with, after the owner check.
    pub enroll_refusal: Option<String>,
    /// Stored launch registrations, by agent pubkey.
    pub registrations: BTreeMap<String, Value>,
    /// Recorded scopes prove retries converge on the same Sandbox.
    pub deploy_roots: Vec<String>,
    /// Commit a deploy, then lose its response once.
    pub lost_deploy_reply: bool,
    pub paseo_enabled: bool,
    pub provisioned_pairing: Option<Value>,
}

impl FakeManager {
    pub fn for_developer(owner: &str) -> Self {
        let manager = Self {
            keys: Keys::generate(),
            inner: Mutex::default(),
        };
        manager.state().developers.insert(owner.to_owned());
        manager
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, ManagerState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn summary(profile: &Value) -> Value {
        json!({
            "version_id": "v",
            "git": {"name": profile["git"]["name"], "email": profile["git"]["email"],
                "signing_key": profile["git"]["ssh_signing_key"].is_string()},
            "github": {"login": profile["github"]["login"], "token": profile["github"]["token"].is_string()},
            "via": {"profile": profile["via"]["profile"].is_string(), "op_token": profile["via"]["op_token"].is_string()},
            "models": profile["models"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
        })
    }

    fn answer(&self, signer: &str, op: Op, payload: &Value, auth: Option<&str>) -> Value {
        let mut state = self.state();
        state.ops.push(op);
        let owner = match (op, auth) {
            (Op::Enroll | Op::Profile, None) => signer.to_owned(),
            (Op::Deploy, Some(tag)) => {
                let agent = nostr::PublicKey::from_hex(signer).unwrap();
                match buzz_sdk::nip_oa::verify_auth_tag(tag, &agent) {
                    Ok(owner) => owner.to_hex(),
                    Err(_) => return json!({"ok": false, "code": "unauthorized_owner"}),
                }
            }
            _ => return json!({"ok": false, "code": "signer_mismatch"}),
        };
        if !state.any_developer && !state.developers.contains(&owner) {
            return json!({"ok": false, "code": "unauthorized_owner"});
        }
        let with_paseo = |mut reply: Value, paseo: &Option<Value>| {
            if let Some(paseo) = paseo {
                reply["paseo"] = paseo.clone();
            }
            reply
        };
        match op {
            Op::Profile => {
                let summary = state.profile.as_ref().map(Self::summary);
                with_paseo(
                    json!({"ok": true, "profile": summary, "paseo_enabled": state.paseo_enabled || state.paseo.is_some()}),
                    &state.paseo,
                )
            }
            Op::Enroll => {
                if let Some(code) = state.enroll_refusal.take() {
                    return json!({"ok": false, "code": code, "error": "refused by the fake"});
                }
                let profile = payload["profile"].clone();
                let checked = match &state.profile {
                    None => profile::validate(&profile),
                    Some(_) => profile::validate_partial(&profile),
                };
                if checked.is_err() || profile["owner_pubkey"] != owner {
                    return json!({"ok": false, "code": "invalid_profile"});
                }
                let merged = match state.profile.take() {
                    None => profile,
                    Some(mut stored) => {
                        merge(&mut stored, &profile);
                        stored
                    }
                };
                state.profile = Some(merged);
                state.versions += 1;
                if state.paseo_enabled && state.paseo.is_none() {
                    state.paseo = state.provisioned_pairing.clone();
                }
                let version = format!("v{}", state.versions);
                with_paseo(json!({"ok": true, "version_id": version}), &state.paseo)
            }
            Op::Deploy => {
                // The real manager's checks on the provider request.
                let request = buzz_backend_kubernetes::provider::parse(payload.clone())
                    .unwrap_or_else(|_| panic!("the manager could not parse the deploy"));
                let buzz_backend_kubernetes::wire::Request::Deploy(deploy) = &request else {
                    panic!("not a deploy");
                };
                buzz_backend_kubernetes::provider::check_sandbox_request(deploy)
                    .unwrap_or_else(|error| panic!("the manager refused the deploy: {error:?}"));
                assert!(state.profile.is_some(), "deploys follow enrollment");
                state.deploy_roots.push(
                    payload["provider_config"]["sandbox"]["thread_root"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
                if let Some(code) = state.failing_deploys.remove(signer) {
                    // The real manager refuses `paseo_store_failed` after it
                    // saved the registration; the other codes, before.
                    if code == "paseo_store_failed" {
                        state
                            .registrations
                            .insert(signer.to_owned(), payload["agent"].clone());
                    }
                    return json!({"ok": false, "code": code, "error": "refused by the fake"});
                }
                state
                    .registrations
                    .insert(signer.to_owned(), payload["agent"].clone());
                json!({"ok": true, "agent_id": "sandbox-1"})
            }
            _ => json!({"ok": false, "code": "unsupported"}),
        }
    }
}

fn merge(stored: &mut Value, partial: &Value) {
    for (key, value) in partial.as_object().into_iter().flatten() {
        match (stored.get_mut(key), value) {
            (Some(Value::Object(into)), Value::Object(from)) => {
                for (field, value) in from {
                    into.insert(field.clone(), value.clone());
                }
            }
            _ => {
                stored[key] = value.clone();
            }
        }
    }
}

impl ControlTransport for &FakeManager {
    async fn exchange<F>(
        &self,
        signer: &Signer,
        request: Event,
        _timeout: Duration,
        mut open: F,
    ) -> Result<Value, CliError>
    where
        F: FnMut(&Event) -> Option<Value>,
    {
        let opened = open_request(&request, &self.keys).expect("encrypted to the manager");
        assert!(!request.content.contains("token"), "content is ciphertext");
        assert_eq!(opened.signer, signer.keys.public_key());
        let response = self.answer(
            &opened.signer.to_hex(),
            opened.request.op,
            &opened.request.payload,
            opened.auth_tag.as_deref(),
        );
        if opened.request.op == Op::Deploy && self.state().lost_deploy_reply {
            self.state().lost_deploy_reply = false;
            return Err(CliError::DeliveryUnknown(
                "manager reply lost after deploy".into(),
            ));
        }
        let id = opened.request.request_id.clone();
        let requester = signer.keys.public_key();
        let now = Timestamp::now();
        let stranger = Keys::generate();
        let candidates = [
            // Not terminal.
            build_reply(
                &self.keys,
                &requester,
                &request.id,
                &Reply::accepted(id.clone()),
                now,
            ),
            // Forged: not the manager's key.
            build_reply(
                &stranger,
                &requester,
                &request.id,
                &Reply::done(id.clone(), json!({"ok": true, "version_id": "forged"})),
                now,
            ),
            // Another request's reply.
            build_reply(
                &self.keys,
                &requester,
                &nostr::EventId::from_slice(&[7u8; 32]).expect("an id"),
                &Reply::done("other", json!({"ok": false})),
                now,
            ),
            build_reply(
                &self.keys,
                &requester,
                &request.id,
                &Reply::done(id, response),
                now,
            ),
        ];
        for candidate in candidates {
            if let Some(response) = open(&candidate.expect("reply builds")) {
                return Ok(response);
            }
        }
        Err(CliError::Other("no reply opened".into()))
    }
}

/// Local tools with scripted results.
#[derive(Default)]
pub struct FakeTools {
    pub gh_token: Option<String>,
    pub identities: BTreeMap<String, GitHubIdentity>,
    pub keys_made: Mutex<u32>,
    /// What the next `gh` signing key additions return, in order; `Ok` after.
    pub gh_results: Mutex<VecDeque<Result<(), &'static str>>>,
    pub gh_added: Mutex<Vec<(String, String)>>,
    pub git_email: String,
}

impl Tools for &FakeTools {
    async fn gh_auth_token(&self) -> Option<Zeroizing<String>> {
        self.gh_token.clone().map(Zeroizing::new)
    }

    async fn github_identity(&self, token: &str) -> Result<GitHubIdentity, String> {
        self.identities
            .get(token)
            .cloned()
            .ok_or_else(|| "GitHub did not accept the token.".into())
    }

    async fn ssh_keygen(&self, comment: &str) -> Result<SigningKey, String> {
        let mut made = self.keys_made.lock().unwrap();
        *made += 1;
        Ok(SigningKey {
            private: Zeroizing::new(format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nPRIVATEKEYMATERIAL{made}\n-----END OPENSSH PRIVATE KEY-----\n"
            )),
            public: format!("ssh-ed25519 AAAAPUBLIC{made} {comment}"),
        })
    }

    async fn gh_add_signing_key(
        &self,
        login: &str,
        _title: &str,
        public: &str,
    ) -> Result<(), &'static str> {
        let result = self
            .gh_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()));
        if result.is_ok() {
            self.gh_added
                .lock()
                .unwrap()
                .push((login.to_owned(), public.to_owned()));
        }
        result
    }

    async fn git_email(&self) -> String {
        self.git_email.clone()
    }

    async fn claude_setup_token(&self) -> bool {
        true
    }
}

/// Every string value in `value`, for secret scans.
pub fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(fields) => fields.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

#[allow(dead_code)]
pub fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// Gateway grants as `buzz_sdk::gateway::Delegation::grant` builds them on
/// `tee8z/agent-gateway-key`: an owner-signed kind 30181 head with `d` and
/// `p` naming the gateway and a NIP-40 expiration.
pub struct FakeGrants;

impl super::super::gateway::GatewayGrants for FakeGrants {
    fn grant(
        &self,
        owner: &Keys,
        gateway: &nostr::PublicKey,
        expires_at: u64,
        created_at: u64,
    ) -> Result<Option<nostr::Event>, String> {
        let content = json!({"version": 1, "gateway": gateway.to_hex(),
            "scope": "remote-agents", "enabled": true});
        nostr::EventBuilder::new(nostr::Kind::Custom(30181), content.to_string())
            .tags([
                nostr::Tag::parse(["d", &gateway.to_hex()]).map_err(|e| e.to_string())?,
                nostr::Tag::parse(["p", &gateway.to_hex()]).map_err(|e| e.to_string())?,
                nostr::Tag::expiration(nostr::Timestamp::from(expires_at)),
            ])
            .custom_created_at(nostr::Timestamp::from(created_at))
            .sign_with_keys(owner)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}
