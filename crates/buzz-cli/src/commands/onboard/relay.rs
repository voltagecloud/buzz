//! The community boundary: the relay's HTTP bridge (`POST /query`,
//! `POST /events`) and its invite endpoints, through the same NIP-98
//! client every other `buzz` command uses.

use nostr::{Event, Keys, Tag};
use serde_json::{json, Value};

use crate::client::BuzzClient;
use crate::error::CliError;

/// Who signs a request: a key, and for an agent its owner's attestation.
#[derive(Clone)]
pub struct Signer {
    /// The signing key.
    pub keys: Keys,
    /// The NIP-OA `auth` tag JSON, for an agent key.
    pub auth_tag: Option<String>,
}

impl Signer {
    /// The owner, who needs no attestation.
    pub fn owner(keys: &Keys) -> Self {
        Self {
            keys: keys.clone(),
            auth_tag: None,
        }
    }
}

/// The community's join policy, when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinPolicy {
    /// The policy revision to accept.
    pub version: String,
    /// Whether joining requires a minimum-age assertion.
    pub age_attestation_required: bool,
}

/// How a claim ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    /// The key joined now.
    Joined,
    /// The key was already a member.
    AlreadyMember,
}

/// What `buzz onboard` asks of the community.
pub trait Relay {
    /// Whether `keys` can read the community: a member's query succeeds,
    /// anyone else's is refused with 401 or 403.
    async fn is_member(&self, keys: &Keys) -> Result<bool, CliError>;

    /// The join policy, `None` when there is none.
    async fn join_policy(&self) -> Result<Option<JoinPolicy>, CliError>;

    /// Accepts the join policy for `code`, returning the relay's receipt.
    async fn accept_policy(
        &self,
        keys: &Keys,
        code: &str,
        policy: &JoinPolicy,
        age_confirmed: bool,
    ) -> Result<String, CliError>;

    /// Claims the invite `code` as `keys`.
    async fn claim(
        &self,
        keys: &Keys,
        code: &str,
        receipt: Option<&str>,
    ) -> Result<Claimed, CliError>;

    /// Events matching `filter`, read as `keys`.
    async fn query(&self, keys: &Keys, filter: Value) -> Result<Vec<Value>, CliError>;

    /// Submits `event`, signed by `signer`; fails unless the relay accepts it.
    async fn publish(&self, signer: &Signer, event: Event) -> Result<(), CliError>;
}

/// The relay's HTTP bridge at `base` (`https://host`).
pub struct HttpRelay {
    base: String,
}

impl HttpRelay {
    /// The bridge of the `wss://` community `relay`.
    pub fn new(relay: &str) -> Self {
        Self {
            base: crate::client::normalize_relay_url(relay),
        }
    }

    fn client(&self, signer: &Signer) -> Result<BuzzClient, CliError> {
        let (tag, json) = match &signer.auth_tag {
            Some(json) => {
                let tag: Tag = buzz_sdk::nip_oa::parse_auth_tag(json)
                    .map_err(|_| CliError::Auth("the agent attestation is malformed".into()))?;
                (Some(tag), Some(json.clone()))
            }
            None => (None, None),
        };
        BuzzClient::new(self.base.clone(), signer.keys.clone(), tag, json)
    }
}

fn denied(error: &CliError) -> bool {
    matches!(
        error,
        CliError::Relay {
            status: 401 | 403,
            ..
        } | CliError::Auth(_)
    )
}

impl Relay for HttpRelay {
    async fn is_member(&self, keys: &Keys) -> Result<bool, CliError> {
        let client = self.client(&Signer::owner(keys))?;
        let filter = json!({"kinds": [0], "authors": [keys.public_key().to_hex()], "limit": 1});
        match client.query(&filter).await {
            Ok(_) => Ok(true),
            Err(error) if denied(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn join_policy(&self) -> Result<Option<JoinPolicy>, CliError> {
        let client = self.client(&Signer::owner(&Keys::generate()))?;
        let body = match client.get_public("/api/join-policy").await {
            Ok(body) => body,
            Err(CliError::Relay { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        parse_join_policy(&body)
    }

    async fn accept_policy(
        &self,
        keys: &Keys,
        code: &str,
        policy: &JoinPolicy,
        age_confirmed: bool,
    ) -> Result<String, CliError> {
        let client = self.client(&Signer::owner(keys))?;
        let body = json!({
            "code": code,
            "policy_version": policy.version,
            "age_confirmed": age_confirmed,
        });
        let reply = client
            .post_json_authed("/api/invites/accept-policy", &body)
            .await?;
        serde_json::from_str::<Value>(&reply)
            .ok()
            .and_then(|value| value.get("receipt")?.as_str().map(str::to_owned))
            .filter(|receipt| !receipt.is_empty())
            .ok_or_else(|| CliError::Other("The relay did not accept the join policy.".into()))
    }

    async fn claim(
        &self,
        keys: &Keys,
        code: &str,
        receipt: Option<&str>,
    ) -> Result<Claimed, CliError> {
        let client = self.client(&Signer::owner(keys))?;
        let mut body = json!({"code": code});
        if let Some(receipt) = receipt {
            body["policy_receipt"] = json!(receipt);
        }
        let reply = client
            .post_json_authed("/api/invites/claim", &body)
            .await
            .map_err(claim_error)?;
        parse_claim(&reply)
    }

    async fn query(&self, keys: &Keys, mut filter: Value) -> Result<Vec<Value>, CliError> {
        // Provisioning decisions must see prior writes, including after a
        // lost acknowledgement or local-state write failure.
        filter
            .as_object_mut()
            .ok_or_else(|| CliError::Other("Provisioning filter must be an object.".into()))?
            .insert("consistency".into(), json!("strong"));
        self.client(&Signer::owner(keys))?
            .query_all_bounded(filter, 500)
            .await
    }

    async fn publish(&self, signer: &Signer, event: Event) -> Result<(), CliError> {
        let reply = self.client(signer)?.submit_event(event).await?;
        accepted(&reply)
    }
}

/// `{"policy": {...}}`, or `{}` for none.
pub fn parse_join_policy(body: &str) -> Result<Option<JoinPolicy>, CliError> {
    let malformed = || CliError::Other("The relay returned an invalid join policy.".into());
    let value: Value = serde_json::from_str(body).map_err(|_| malformed())?;
    let Some(policy) = value.get("policy").filter(|policy| !policy.is_null()) else {
        return Ok(None);
    };
    Ok(Some(JoinPolicy {
        version: policy["version"].as_str().ok_or_else(malformed)?.to_owned(),
        age_attestation_required: policy["age_attestation_required"]
            .as_bool()
            .ok_or_else(malformed)?,
    }))
}

/// `{"status":"joined"|"already_member", …}`; nothing else counts.
pub fn parse_claim(body: &str) -> Result<Claimed, CliError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| CliError::Other("The relay returned an invalid invite response.".into()))?;
    match value.get("status").and_then(Value::as_str) {
        Some("joined") => Ok(Claimed::Joined),
        Some("already_member") => Ok(Claimed::AlreadyMember),
        _ => Err(CliError::Other(
            "The relay returned an invalid invite response.".into(),
        )),
    }
}

/// The relay's invite refusals, worded as the Buzz app words them.
fn claim_error(error: CliError) -> CliError {
    let CliError::Relay { status, body } = &error else {
        return error;
    };
    let text = match (status, body.as_str()) {
        (429, _) => "Too many invite attempts. Wait a minute and run buzz onboard again.",
        (_, "invite_expired") => "This invite code has expired. Ask for a new one.",
        (_, "invite_exhausted") => "This invite has reached its use limit. Ask for a new invite.",
        (_, "invite_invalid") => "This invite code is not valid for this community.",
        (_, "join_policy_required") => "This community requires current policy acceptance.",
        _ => return error,
    };
    CliError::Auth(text.into())
}

/// The relay's write receipt must say `accepted: true`.
pub fn accepted(reply: &str) -> Result<(), CliError> {
    let value: Value = serde_json::from_str(reply).unwrap_or(Value::Null);
    if value.get("accepted").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no receipt");
        Err(CliError::Other(format!(
            "the relay did not accept the event: {message}"
        )))
    }
}
