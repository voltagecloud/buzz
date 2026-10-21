//! Joining a community (`docs/port/relay.md` §1.6–1.7): invite links in
//! both forms, the NIP-11 and join-policy check, the membership probe, the
//! invite claim, the profile published on joining, and the journal that
//! lets an interrupted join resume.
//!
//! The parsers are pure. [`inspect`], [`inspect_profile`], [`claim`] and
//! [`publish_profile`] run one HTTP step each over a [`RelayHttp`] for the
//! joining identity, before any session exists. Ports `invite-link.ts`,
//! `native-api.ts`, `join-journal.ts` and the order of `CommunityDialog.tsx`.

use crate::account::{self, ProfileEdit};
use crate::event::{Verifier, kinds};
use crate::http::{HttpError, RelayHttp};
use crate::origin::RelayOrigin;
use crate::outbox::{PublishError, accept_publish};
use crate::store::Store;
use buzz_identity::Purpose;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

/// Each join request's budget (`native-api.ts`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_LINK: usize = 4096;
const MAX_CODE: usize = 256;

/// What the user pasted into "Add a community".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommunityInput {
    /// A bare relay address: open it as an existing member.
    Relay(RelayOrigin),
    /// An invite: claim `code` at `origin`. Only an intent; the user still
    /// confirms.
    Invite { origin: RelayOrigin, code: String },
}

impl CommunityInput {
    pub fn origin(&self) -> &RelayOrigin {
        match self {
            Self::Relay(origin) | Self::Invite { origin, .. } => origin,
        }
    }
}

/// `/^[a-zA-Z0-9._-]{1,256}$/`.
pub fn valid_code(code: &str) -> bool {
    (1..=MAX_CODE).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Parses a relay URL, `buzz://join?relay=…&code=…[&policy_receipt=…]`,
/// or the relay's share link `https://host/invite/<code>`.
pub fn parse_input(input: &str) -> Result<CommunityInput, String> {
    let input = input.trim();
    if input.len() > MAX_LINK {
        return Err("This link is too long.".into());
    }
    if input.to_ascii_lowercase().starts_with("buzz:") {
        return parse_deep_link(input).ok_or_else(|| "This invite link is not valid.".to_owned());
    }
    if let Some(invite) = parse_share_link(input) {
        return Ok(invite);
    }
    RelayOrigin::parse(input)
        .map(CommunityInput::Relay)
        .map_err(|error| error.to_string())
}

/// `invite-link.ts`. The policy receipt is validated, then discarded: the
/// join asks for a fresh one.
fn parse_deep_link(input: &str) -> Option<CommunityInput> {
    let rest = input.get("buzz://join".len()..)?;
    if !input[..11].eq_ignore_ascii_case("buzz://join")
        || !(rest.starts_with('?') || rest.starts_with("/?"))
    {
        return None;
    }
    let url = Url::parse(input).ok()?;
    if url.host_str() != Some("join")
        || !matches!(url.path(), "" | "/")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let (mut relay, mut code, mut receipts) = (Vec::new(), Vec::new(), 0);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "relay" => relay.push(value.into_owned()),
            "code" => code.push(value.into_owned()),
            "policy_receipt" if !value.is_empty() => receipts += 1,
            "policy_receipt" => return None,
            _ => {}
        }
    }
    let ([relay], [code]) = (relay.as_slice(), code.as_slice()) else {
        return None;
    };
    if receipts > 1 || code.is_empty() || code.len() > MAX_CODE {
        return None;
    }
    Some(CommunityInput::Invite {
        origin: RelayOrigin::parse(relay).ok()?,
        code: code.clone(),
    })
}

/// `https://host/invite/<code>` → community `https://host`, that code.
fn parse_share_link(input: &str) -> Option<CommunityInput> {
    let url = Url::parse(input).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.path().strip_prefix("/invite/")?;
    let code = path.strip_suffix('/').unwrap_or(path);
    if !valid_code(code) {
        return None;
    }
    let mut origin = url.clone();
    origin.set_path("");
    let origin = RelayOrigin::parse(origin.as_str()).ok()?;
    Some(CommunityInput::Invite {
        origin,
        code: code.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Relay information and join policy

/// What the relay says about itself before joining.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinInfo {
    /// The NIP-11 name, unless it is the default "Buzz Relay".
    pub name: Option<String>,
    pub policy: Option<JoinPolicy>,
}

/// `GET /api/join-policy`'s `policy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinPolicy {
    pub terms: Option<String>,
    pub privacy: Option<String>,
    pub age_required: bool,
    pub version: String,
}

/// `{}` means no policy; a malformed policy is an error, never ignored.
pub fn parse_policy(body: &Value) -> Result<Option<JoinPolicy>, String> {
    let Some(policy) = body.get("policy") else {
        return Ok(None);
    };
    let text = |key: &str| {
        policy
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned)
    };
    match (
        policy.get("version").and_then(Value::as_str),
        policy
            .get("age_attestation_required")
            .and_then(Value::as_bool),
    ) {
        (Some(version), Some(age_required)) => Ok(Some(JoinPolicy {
            terms: text("terms_markdown"),
            privacy: text("privacy_markdown"),
            age_required,
            version: version.to_owned(),
        })),
        _ => Err("This community's join policy could not be read.".into()),
    }
}

fn nip11_name(info: &Value) -> Option<String> {
    info.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "Buzz Relay")
        .map(str::to_owned)
}

async fn timed<T>(future: impl Future<Output = Result<T, HttpError>>) -> Result<T, HttpError> {
    tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| Err(HttpError::Transport("timed out".into())))
}

/// NIP-11 and the join policy, read together. A missing policy route is no
/// policy.
pub async fn inspect(http: &RelayHttp) -> Result<JoinInfo, String> {
    let (info, policy) = tokio::join!(
        timed(http.information()),
        timed(http.get_public("/api/join-policy"))
    );
    let info =
        info.map_err(|_| "This relay could not be reached. Check the address.".to_owned())?;
    if info.get("self").and_then(Value::as_str).is_none() {
        return Err("Relay did not advertise its identity".into());
    }
    let policy = match policy {
        Ok(body) => parse_policy(&body)?,
        Err(HttpError::Status { status: 404, .. }) => None,
        Err(_) => return Err("This community's join policy could not be read.".into()),
    };
    Ok(JoinInfo {
        name: nip11_name(&info),
        policy,
    })
}

/// Whether the identity is already a member: a strong read of its own
/// profile succeeds for members and is refused (401/403) otherwise.
pub async fn probe(http: &RelayHttp) -> Result<bool, String> {
    Ok(inspect_profile(http).await?.is_some())
}

/// The viewer's kind 0 in one community, as the join's profile step sees
/// it (`inspectProfile`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommunityProfile {
    /// Name, picture and about as published; `None` without a kind 0.
    pub profile: Option<ProfileEdit>,
    /// The kind 0's content, so a publish keeps fields it does not edit.
    pub existing: Option<String>,
    pub created_at: Option<u64>,
}

impl CommunityProfile {
    /// Whether publishing `profile` would change nothing (`profilesEqual`).
    pub fn matches(&self, profile: &ProfileEdit) -> bool {
        self.profile.as_ref().is_some_and(|current| {
            current.name == profile.name
                && current.picture == profile.picture
                && current.about == profile.about
        })
    }
}

/// The newest signed kind 0 by `viewer` in a `/query` answer, read as the
/// profile step shows it: `display_name`, else `name`.
pub fn community_profile(events: &[Value], viewer: &str) -> CommunityProfile {
    let verifier = Verifier::default();
    let verified: Vec<_> = events
        .iter()
        .filter_map(|value| verifier.verify_json(value).ok())
        .collect();
    let Some(event) = account::newest_profile(&verified, viewer) else {
        return CommunityProfile::default();
    };
    let fields: serde_json::Map<String, Value> = match serde_json::from_str(&event.content) {
        Ok(Value::Object(fields)) => fields,
        _ => Default::default(),
    };
    let text = |key: &str| {
        fields
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    CommunityProfile {
        profile: Some(ProfileEdit {
            name: text("display_name")
                .or_else(|| text("name"))
                .unwrap_or_default(),
            picture: text("picture").unwrap_or_default(),
            about: text("about").unwrap_or_default(),
        }),
        existing: Some(event.content.clone()),
        created_at: Some(event.created_at_secs()),
    }
}

/// A strong read of the viewer's own kind 0. `None` when the relay
/// refuses it (401/403): the identity is not a member.
pub async fn inspect_profile(http: &RelayHttp) -> Result<Option<CommunityProfile>, String> {
    let viewer = http.identity().pubkey_hex();
    let filter = json!({
        "kinds": [kinds::PROFILE],
        "authors": [viewer],
        "limit": 5,
        "consistency": "strong",
    });
    let filter = filter.as_object().expect("object").clone();
    match timed(http.query_raw(&[filter])).await {
        Ok(events) => Ok(Some(community_profile(&events, &viewer))),
        Err(HttpError::Status {
            status: 401 | 403, ..
        }) => Ok(None),
        Err(error) => Err(format!("Membership could not be checked: {error}")),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Publishes the viewer's kind 0 in a community just joined (§1.6 step
/// 5): `found`'s other fields are kept, `name` and `display_name` both
/// take the name. The caller re-reads to confirm it is current.
pub async fn publish_profile(
    http: &RelayHttp,
    profile: &ProfileEdit,
    found: &CommunityProfile,
) -> Result<(), String> {
    let template =
        account::profile_template(profile, found.existing.as_deref(), found.created_at, now())
            .map_err(|error| error.to_string())?;
    let identity = http.identity();
    let unsigned = identity
        .template(
            Purpose::Content,
            template.kind,
            template.created_at,
            template.tags,
            template.content,
        )
        .map_err(|error| error.to_string())?;
    let event = identity
        .sign(Purpose::Content, unsigned)
        .map_err(|error| error.to_string())?;
    let body = serde_json::to_string(&event).expect("events serialize");
    match accept_publish(timed(http.post("/events", body)).await, &event.id) {
        Ok(_) => Ok(()),
        Err(PublishError::Rejected(message) | PublishError::Uncertain(message)) => Err(message),
    }
}

// ---------------------------------------------------------------------------
// Join journal (`join-journal.ts`)

/// Viewer preference holding unfinished joins.
const JOURNAL: &str = "community-joins";
const UNREADABLE: &str =
    "Could not read unfinished community setup. Restore device storage and try again.";
const UNSAVED: &str = "Could not save community setup on this device. Free storage and try again.";

/// One join in progress: public progress only. Invite codes, policy
/// receipts and keys are never journaled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingJoin {
    /// A fresh id per step, so a late completion of an older step can
    /// tell it was superseded.
    pub id: String,
    /// The community's origin.
    pub community: String,
    /// The profile draft, once the profile step was submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileEdit>,
}

/// The viewer's unfinished joins, one per community, in the local store.
/// Written before a claim and before a profile publish, removed once the
/// community is saved; reopening the join resumes from it.
#[derive(Clone)]
pub struct JoinJournal {
    store: Store,
    viewer: String,
}

fn origin_key(community: &str) -> Result<String, String> {
    RelayOrigin::parse(community)
        .map(|origin| origin.to_string())
        .map_err(|error| error.to_string())
}

impl JoinJournal {
    pub fn new(store: Store, viewer: impl Into<String>) -> Self {
        Self {
            store,
            viewer: viewer.into(),
        }
    }

    /// Every entry, or an error for storage that cannot be read; a corrupt
    /// journal is never overwritten.
    fn read(&self) -> Result<Vec<PendingJoin>, String> {
        let Some(json) = self
            .store
            .viewer_pref(&self.viewer, JOURNAL)
            .map_err(|_| UNREADABLE.to_owned())?
        else {
            return Ok(Vec::new());
        };
        let mut entries: Vec<PendingJoin> =
            serde_json::from_str(&json).map_err(|_| UNREADABLE.to_owned())?;
        for entry in &mut entries {
            entry.community = origin_key(&entry.community).map_err(|_| UNREADABLE.to_owned())?;
        }
        Ok(entries)
    }

    fn save(&self, entries: &[PendingJoin]) -> Result<(), String> {
        let json = serde_json::to_string(entries).expect("journal serializes");
        self.store
            .set_viewer_pref(&self.viewer, JOURNAL, &json)
            .map_err(|_| UNSAVED.to_owned())
    }

    /// The join started last, to resume.
    pub fn latest(&self) -> Result<Option<PendingJoin>, String> {
        Ok(self.read()?.pop())
    }

    pub fn get(&self, community: &str) -> Result<Option<PendingJoin>, String> {
        let community = origin_key(community)?;
        Ok(self
            .read()?
            .into_iter()
            .rfind(|entry| entry.community == community))
    }

    /// Starts a step for `community`, replacing its entry. Without a
    /// `profile`, an earlier draft carries over.
    pub fn begin(
        &self,
        community: &str,
        profile: Option<ProfileEdit>,
    ) -> Result<PendingJoin, String> {
        let mut entries = self.read()?;
        let community = origin_key(community)?;
        let profile = profile.or_else(|| {
            entries
                .iter()
                .rfind(|entry| entry.community == community)
                .and_then(|entry| entry.profile.clone())
        });
        let entry = PendingJoin {
            id: uuid::Uuid::new_v4().to_string(),
            community,
            profile,
        };
        entries.retain(|item| item.community != entry.community);
        entries.push(entry.clone());
        self.save(&entries)?;
        Ok(entry)
    }

    /// The step is still the latest for its community.
    pub fn current(&self, entry: &PendingJoin) -> bool {
        self.read().is_ok_and(|entries| {
            entries
                .iter()
                .any(|item| item.id == entry.id && item.community == entry.community)
        })
    }

    /// The community is saved: the join is over.
    pub fn finish(&self, entry: &PendingJoin) -> Result<(), String> {
        let mut entries = self.read()?;
        entries.retain(|item| item.id != entry.id);
        self.save(&entries)
    }
}

/// How a claim ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    Joined,
    AlreadyMember,
}

/// Accepts the policy (when there is one) and claims the invite.
/// `age_confirmed` answers the policy's age question.
pub async fn claim(
    http: &RelayHttp,
    code: &str,
    policy: Option<&JoinPolicy>,
    age_confirmed: bool,
) -> Result<Claimed, String> {
    if !valid_code(code) {
        return Err("This invite code is not valid.".into());
    }
    let receipt = match policy {
        Some(policy) => {
            let body = json!({
                "code": code,
                "policy_version": policy.version,
                "age_confirmed": age_confirmed,
            });
            let bytes = timed(http.post("/api/invites/accept-policy", body.to_string()))
                .await
                .map_err(claim_error)?;
            let receipt = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| value.get("receipt")?.as_str().map(str::to_owned))
                .filter(|receipt| !receipt.is_empty())
                .ok_or("The relay did not accept the join policy.")?;
            Some(receipt)
        }
        None => None,
    };
    let mut body = json!({"code": code});
    if let Some(receipt) = receipt {
        body["policy_receipt"] = json!(receipt);
    }
    let bytes = timed(http.post("/api/invites/claim", body.to_string()))
        .await
        .map_err(claim_error)?;
    parse_claim(&bytes)
}

/// `{"status":"joined"|"already_member", …}`; nothing else counts.
pub fn parse_claim(body: &[u8]) -> Result<Claimed, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| "The relay returned an invalid invite response.")?;
    match value.get("status").and_then(Value::as_str) {
        Some("joined") => Ok(Claimed::Joined),
        Some("already_member") => Ok(Claimed::AlreadyMember),
        _ => Err("The relay returned an invalid invite response.".into()),
    }
}

/// The relay's refusal codes, worded for people.
pub fn claim_error(error: HttpError) -> String {
    match &error {
        HttpError::Status { status: 429, .. } => {
            "Too many invite attempts. Wait a minute and try again.".into()
        }
        HttpError::Status { message, .. } => match message.as_str() {
            // `CLAIM_REFUSALS` (`CommunityDialog.tsx`).
            "invite_expired" => "This invite code has expired — ask for a new one.".into(),
            "invite_exhausted" => {
                "This invite has reached its use limit. Ask for a new invite.".into()
            }
            "invite_invalid" => "This invite code is not valid for this community.".into(),
            "join_policy_required" => "This community requires current policy acceptance. \
                 Go back and reopen the relay to review it."
                .into(),
            "join_policy_not_accepted" => {
                "The community policy changed. Go back and reopen the relay to review it.".into()
            }
            _ => format!("The invite could not be claimed: {error}"),
        },
        _ => format!("The invite could not be claimed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(origin: &str, code: &str) -> CommunityInput {
        CommunityInput::Invite {
            origin: RelayOrigin::parse(origin).unwrap(),
            code: code.into(),
        }
    }

    #[test]
    fn parses_both_invite_forms_and_bare_relays() {
        assert_eq!(
            parse_input("buzz://join?relay=wss%3A%2F%2Frelay.example.com&code=v2.abc_1-2").unwrap(),
            invite("wss://relay.example.com", "v2.abc_1-2")
        );
        assert_eq!(
            parse_input("buzz://join/?code=x&relay=https://relay.example.com&policy_receipt=r")
                .unwrap(),
            invite("wss://relay.example.com", "x")
        );
        assert_eq!(
            parse_input(" https://Relay.Example.com/invite/abc ").unwrap(),
            invite("wss://relay.example.com", "abc")
        );
        assert_eq!(
            parse_input("http://127.0.0.1:53000/invite/abc/").unwrap(),
            invite("ws://127.0.0.1:53000", "abc")
        );
        assert_eq!(
            parse_input("wss://relay.example.com").unwrap(),
            CommunityInput::Relay(RelayOrigin::parse("wss://relay.example.com").unwrap())
        );
    }

    #[test]
    fn refuses_malformed_invites() {
        for bad in [
            "buzz://join?relay=wss://a.example&code=",
            "buzz://join?relay=wss://a.example",
            "buzz://join?relay=wss://a.example&code=x&code=y",
            "buzz://join?relay=wss://a.example/path&code=x",
            "buzz://join?relay=wss://a.example&code=x&policy_receipt=",
            "buzz://join?relay=wss://a.example&code=x&policy_receipt=a&policy_receipt=b",
            "buzz://join/extra?relay=wss://a.example&code=x",
            "buzz://channel?relay=wss://a.example&code=x",
            "buzz://join?relay=wss://a.example&code=x#frag",
        ] {
            assert!(parse_input(bad).is_err(), "{bad}");
        }
        assert!(parse_input("https://a.example/invite/bad code").is_err());
        assert!(parse_input("https://a.example/invite/x?y=1").is_err());
        assert!(
            parse_input(&format!(
                "buzz://join?relay=wss://a&code={}",
                "x".repeat(4100)
            ))
            .is_err()
        );
    }

    #[test]
    fn reads_the_join_policy() {
        assert_eq!(parse_policy(&json!({})), Ok(None));
        let policy = parse_policy(&json!({"policy": {
            "terms_markdown": "Be kind", "privacy_markdown": null,
            "age_attestation_required": true, "version": "3"
        }}))
        .unwrap()
        .unwrap();
        assert_eq!(policy.terms.as_deref(), Some("Be kind"));
        assert!(policy.privacy.is_none() && policy.age_required);
        assert_eq!(policy.version, "3");
        assert!(parse_policy(&json!({"policy": {"version": 3}})).is_err());
    }

    #[test]
    fn accepts_only_known_claim_statuses() {
        assert_eq!(parse_claim(br#"{"status":"joined"}"#), Ok(Claimed::Joined));
        assert_eq!(
            parse_claim(br#"{"status":"already_member","role":"member"}"#),
            Ok(Claimed::AlreadyMember)
        );
        assert!(parse_claim(br#"{"status":"pending"}"#).is_err());
        let expired = HttpError::Status {
            status: 403,
            message: "invite_expired".into(),
            retry_after: None,
            not_sent: false,
        };
        assert!(claim_error(expired).contains("expired"));
    }

    #[test]
    fn journals_one_join_per_community_without_secrets() {
        let store = Store::in_memory().unwrap();
        let journal = JoinJournal::new(store.clone(), "viewer");
        assert_eq!(journal.latest().unwrap(), None);
        let first = journal.begin("wss://a.example", None).unwrap();
        let other = journal.begin("https://b.example/", None).unwrap();
        assert_eq!(other.community, "wss://b.example");
        assert_eq!(journal.latest().unwrap(), Some(other.clone()));
        assert!(journal.current(&first));

        // A new step replaces the community's entry and keeps its draft.
        let draft = ProfileEdit {
            name: "Ada".into(),
            ..Default::default()
        };
        let named = journal
            .begin("wss://a.example", Some(draft.clone()))
            .unwrap();
        assert!(!journal.current(&first), "superseded");
        let resumed = journal.begin("wss://a.example", None).unwrap();
        assert!(!journal.current(&named));
        assert_eq!(resumed.profile, Some(draft));
        assert_eq!(
            journal.get("wss://a.example/").unwrap(),
            Some(resumed.clone())
        );
        let stored = store.viewer_pref("viewer", JOURNAL).unwrap().unwrap();
        assert!(!stored.contains("code") && !stored.contains("receipt"));

        journal.finish(&resumed).unwrap();
        assert_eq!(journal.get("wss://a.example").unwrap(), None);
        assert_eq!(journal.latest().unwrap(), Some(other));
        // Each viewer has their own journal.
        assert_eq!(
            JoinJournal::new(store.clone(), "someone").latest().unwrap(),
            None
        );

        // A corrupt journal is an error, never silently replaced.
        store.set_viewer_pref("viewer", JOURNAL, "{").unwrap();
        assert!(journal.latest().is_err());
        assert!(journal.begin("wss://a.example", None).is_err());
        assert_eq!(
            store.viewer_pref("viewer", JOURNAL).unwrap().as_deref(),
            Some("{")
        );
    }

    #[test]
    fn reads_the_profile_the_join_would_replace() {
        let identity = buzz_identity::Identity::ephemeral();
        let viewer = identity.pubkey_hex();
        let sign = |content: &str, at: u64| {
            let unsigned = identity
                .template(Purpose::Content, 0, at, Vec::new(), content.into())
                .unwrap();
            serde_json::to_value(identity.sign(Purpose::Content, unsigned).unwrap()).unwrap()
        };
        let events = [
            sign(r#"{"name":"old"}"#, 10),
            sign(
                r#"{"name":"ada","display_name":" Ada ","about":"Hi","nip05":"a@x"}"#,
                20,
            ),
        ];
        let found = community_profile(&events, &viewer);
        let profile = found.profile.clone().unwrap();
        assert_eq!(
            (profile.name.as_str(), profile.about.as_str()),
            ("Ada", "Hi")
        );
        assert_eq!(found.created_at, Some(20));
        assert!(found.existing.as_deref().unwrap().contains("nip05"));
        assert!(found.matches(&profile));
        assert!(!found.matches(&ProfileEdit {
            about: String::new(),
            ..profile
        }));
        let none = community_profile(&[], &viewer);
        assert_eq!(none, CommunityProfile::default());
        assert!(!none.matches(&ProfileEdit::default()));
    }

    #[test]
    fn hides_the_default_relay_name() {
        assert_eq!(nip11_name(&json!({"name": "Buzz Relay"})), None);
        assert_eq!(nip11_name(&json!({"name": " Team "})), Some("Team".into()));
    }
}
