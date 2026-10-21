//! Agent types compiled into the app, buzz-app's `src/agent-plugins`.

pub mod claude_code;
pub mod codex;
pub mod harness;
pub mod managed;
mod opened;
pub mod owner_relay;
pub mod process;
mod provider;
#[cfg(feature = "voltage-sandbox")]
pub mod remote_workspace;
pub(crate) mod tabs;
mod transcript_view;

use crate::agent_activity::local::{WorkHandle, WorkOutcome};
use crate::agents2::attention::EventData;
use crate::buzz_mcp::BuzzEvent;
use futures::future::BoxFuture;
use std::sync::Arc;

/// Keeps a delivery counted as working until the future settles with the
/// request's outcome: its turn outlives `run`, which returns once the turn
/// is handed over.
pub type HoldUntil = Arc<dyn Fn(BoxFuture<'static, WorkOutcome>) + Send + Sync>;

/// A delivery's [`HoldUntil`], from its working hold.
fn hold_until(working: crate::agents2::runtime::Working) -> HoldUntil {
    Arc::new(move |done| working.until(done))
}

/// A mention's local work row, which the harness moves along: declined,
/// acknowledged, superseded and its steps (`Delivery`'s `ignored`,
/// `acknowledged`, `superseded` and `step`). Watches and timers have none.
fn activity(delivery: &crate::agents2::runtime::Delivery) -> Option<WorkHandle> {
    match delivery.trigger {
        crate::agents2::runtime::Trigger::Mention { .. } => delivery.working.work(),
        _ => None,
    }
}

/// The event an agent was woken for, as the Buzz tools take it.
fn event_of(event: &EventData) -> BuzzEvent {
    BuzzEvent {
        id: event.id.clone(),
        pubkey: event.pubkey.clone(),
        created_at: event.created_at,
        kind: u32::from(event.kind),
        tags: event.tags.clone(),
        content: event.content.clone(),
        sig: String::new(),
    }
}

/// The tags of a reply to `event` in `channel`: in its thread, flat, at
/// the thread's root, and to `to`, the agent's owner. Tagging whoever
/// asked could be a sibling agent, which wakes it, and two agents failing
/// alike would wake each other without end.
fn reply_tags(channel: &str, event: &BuzzEvent, to: &str) -> Vec<Vec<String>> {
    let root = crate::buzz_mcp::tools::root_of(event);
    vec![
        vec!["h".to_string(), channel.to_string()],
        vec!["e".into(), root, String::new(), "reply".into()],
        vec!["p".into(), to.to_owned()],
    ]
}

/// Whether an agent answers `event`: its owner's, or one by another agent
/// the owner attested (NIP-OA). Both compiled-in types answer nobody else.
pub(crate) fn from_owner(event: &BuzzEvent, owner: &str) -> bool {
    event.pubkey == owner
        || u16::try_from(event.kind).ok().and_then(|kind| {
            buzz_relay_client::profiles::attested_owner_of(
                &event.pubkey,
                kind,
                event.created_at,
                &event.tags,
            )
        }) == Some(owner.to_owned())
}

/// The owner of a community workflow's message (`workflow-attribution.ts`):
/// kind 9 signed by the community itself, marked once as a workflow and
/// naming one owner. The caller has checked its signature.
fn workflow_owner(event: &BuzzEvent, authority: Option<&str>) -> Option<String> {
    if authority != Some(event.pubkey.as_str()) || event.kind != 9 {
        return None;
    }
    let named = |name: &str| {
        event
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some(name))
            .collect::<Vec<_>>()
    };
    let (markers, owners) = (named("buzz:workflow"), named("buzz:workflow-owner"));
    match (markers.as_slice(), owners.as_slice()) {
        ([marker], [owner])
            if marker.len() == 2
                && marker[1] == "true"
                && owner.len() == 2
                && crate::buzz_mcp::tools::hex(&owner[1]) =>
        {
            Some(owner[1].clone())
        }
        _ => None,
    }
}

/// Delegated mention provenance: the owner of a workflow message that
/// explicitly targets `agent`. Ordinary `p` tags can come from template
/// substitutions; only owner-authored targets grant prompt authority.
pub(crate) fn workflow_mention_owner(
    event: &BuzzEvent,
    authority: Option<&str>,
    agent: &str,
) -> Option<String> {
    let owner = workflow_owner(event, authority)?;
    let mut targets = std::collections::HashSet::new();
    for tag in event
        .tags
        .iter()
        .filter(|tag| tag.first().map(String::as_str) == Some("buzz:workflow-mention"))
    {
        let target = tag
            .get(1)
            .filter(|target| crate::buzz_mcp::tools::hex(target));
        match target {
            Some(target) if tag.len() == 2 && targets.insert(target.clone()) => {}
            _ => return None,
        }
    }
    targets.contains(agent).then_some(owner)
}

/// [`from_owner`], or a verified workflow mention of `agent` in its own
/// community delegated by the owner, or by an agent whose signed profile
/// the owner attested.
pub(crate) async fn from_owner_or_workflow(
    event: &BuzzEvent,
    owner: &str,
    agent: &str,
    relay: Option<&Arc<dyn owner_relay::OwnerRelay>>,
) -> bool {
    if from_owner(event, owner) {
        return true;
    }
    let Some(relay) = relay else {
        return false;
    };
    let Some(creator) = workflow_mention_owner(event, relay.authority().as_deref(), agent) else {
        return false;
    };
    // A missing or failed profile read is no verdict; a later mention can
    // try again.
    creator == owner || relay.profile_owner(&creator).await.as_deref() == Some(owner)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use nostr::prelude::{Keys, ToBech32};
    use std::collections::HashMap;

    /// An owner, an agent it attested, and that attestation's `auth` tag.
    pub(crate) fn attested() -> (String, String, Vec<String>) {
        let (owner, agent) = (Keys::generate(), Keys::generate());
        let nsec = owner.secret_key().to_bech32().unwrap();
        let identity = buzz_identity::Identity::from_nsec(&nsec).unwrap();
        let auth = identity.attest_agent(&agent.public_key().to_hex()).unwrap();
        (
            owner.public_key().to_hex(),
            agent.public_key().to_hex(),
            serde_json::from_str(&auth).unwrap(),
        )
    }

    #[test]
    fn answers_the_owner_and_agents_the_owner_attested_nobody_else() {
        let (owner, agent, auth) = attested();
        let from = |pubkey: &str, tags: Vec<Vec<String>>| BuzzEvent {
            pubkey: pubkey.into(),
            kind: 9,
            created_at: 100,
            tags,
            ..Default::default()
        };
        assert!(from_owner(&from(&owner, vec![]), &owner));
        assert!(from_owner(&from(&agent, vec![auth.clone()]), &owner));
        assert!(!from_owner(&from(&agent, vec![]), &owner));
        // Another key cannot borrow the agent's attestation, nor claim an
        // owner.
        assert!(!from_owner(
            &from(&"e".repeat(64), vec![auth.clone()]),
            &owner
        ));
        let forged = vec!["auth".into(), owner.clone(), String::new(), "0".repeat(128)];
        assert!(!from_owner(&from(&agent, vec![forged]), &owner));
    }

    #[test]
    fn a_reply_tags_the_owner_not_whoever_asked() {
        let event = BuzzEvent {
            id: "1".repeat(64),
            pubkey: "a".repeat(64),
            created_at: 1,
            kind: 9,
            tags: vec![vec!["h".into(), "c".into()]],
            content: "hi".into(),
            sig: String::new(),
        };
        let tags = reply_tags("c", &event, &"f".repeat(64));
        assert_eq!(tags.last(), Some(&vec!["p".to_string(), "f".repeat(64)]));
        assert!(!tags.iter().any(|tag| tag.get(1) == Some(&event.pubkey)));
    }

    fn workflow(signer: &str, extra: &[&[&str]]) -> BuzzEvent {
        let mut tags: Vec<Vec<String>> = vec![
            vec!["buzz:workflow".into(), "true".into()],
            vec!["buzz:workflow-owner".into(), "c".repeat(64)],
        ];
        tags.extend(
            extra
                .iter()
                .map(|tag| tag.iter().map(|part| part.to_string()).collect()),
        );
        BuzzEvent {
            pubkey: signer.into(),
            kind: 9,
            created_at: 100,
            tags,
            ..Default::default()
        }
    }

    #[test]
    fn delegates_only_to_explicit_canonical_workflow_targets() {
        let (relay, other, creator) = ("r".repeat(64), "b".repeat(64), "c".repeat(64));
        let relay = relay.replace('r', "1");
        let targets: &[&[&str]] = &[
            &["buzz:workflow-mention", &other],
            &["buzz:workflow-mention", &creator],
        ];
        let event = workflow(&relay, targets);
        assert_eq!(
            workflow_mention_owner(&event, Some(&relay), &other),
            Some(creator.clone())
        );
        assert_eq!(workflow_mention_owner(&event, Some(&relay), &relay), None);
        assert_eq!(workflow_mention_owner(&event, None, &other), None);
        assert_eq!(workflow_mention_owner(&event, Some(&other), &other), None);
        let signed_elsewhere = workflow(&other, targets);
        assert_eq!(
            workflow_mention_owner(&signed_elsewhere, Some(&relay), &other),
            None
        );
        let upper = other.to_uppercase();
        for mentions in [
            vec![],
            vec![vec!["p", other.as_str()]],
            vec![vec!["buzz:workflow-mention", creator.as_str()]],
            vec![vec!["buzz:workflow-mention", other.as_str(), "extra"]],
            vec![vec!["buzz:workflow-mention", upper.as_str()]],
            vec![
                vec!["buzz:workflow-mention", other.as_str()],
                vec!["buzz:workflow-mention", other.as_str()],
            ],
            vec![
                vec!["buzz:workflow-mention", other.as_str()],
                vec!["buzz:workflow-mention"],
            ],
            vec![
                vec!["buzz:workflow-mention", other.as_str()],
                vec!["buzz:workflow-mention", "bad"],
            ],
        ] {
            let mentions: Vec<&[&str]> = mentions.iter().map(Vec::as_slice).collect();
            let event = workflow(&relay, &mentions);
            assert_eq!(
                workflow_mention_owner(&event, Some(&relay), &other),
                None,
                "{mentions:?}"
            );
        }
    }

    /// A community with a workflow authority and one creator profile.
    struct Community {
        authority: String,
        profiles: HashMap<String, String>,
        reads: std::sync::Mutex<usize>,
    }

    impl owner_relay::OwnerRelay for Community {
        fn channel(&self, _: &str) -> Option<owner_relay::ChannelInfo> {
            None
        }
        fn read(
            &self,
            _: Vec<serde_json::Value>,
        ) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn names(
            &self,
            _: Vec<String>,
            _: Vec<String>,
        ) -> BoxFuture<'static, HashMap<String, String>> {
            Box::pin(async { HashMap::new() })
        }
        fn memories(
            &self,
            _: &str,
        ) -> BoxFuture<'static, Result<Vec<crate::buzz_mcp::Memory>, String>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn authority(&self) -> Option<String> {
            Some(self.authority.clone())
        }
        fn profile_owner(&self, pubkey: &str) -> BoxFuture<'static, Option<String>> {
            *self.reads.lock().unwrap() += 1;
            let owner = self.profiles.get(pubkey).cloned();
            Box::pin(async move { owner })
        }
    }

    #[tokio::test]
    async fn accepts_a_workflow_mention_delegated_by_the_owner_or_an_agent_it_attested() {
        let (owner, agent, creator, stranger) = (
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            "e".repeat(64),
        );
        let authority = "1".repeat(64);
        let relay: Arc<dyn owner_relay::OwnerRelay> = Arc::new(Community {
            authority: authority.clone(),
            profiles: HashMap::from([(creator.clone(), owner.clone())]),
            reads: std::sync::Mutex::new(0),
        });
        let prompt = workflow(&authority, &[&["buzz:workflow-mention", &agent]]);
        assert!(from_owner_or_workflow(&prompt, &owner, &agent, Some(&relay)).await);
        assert!(!from_owner_or_workflow(&prompt, &stranger, &agent, Some(&relay)).await);
        // Without its community, or naming only by `p`, it is not trusted.
        assert!(!from_owner_or_workflow(&prompt, &owner, &agent, None).await);
        let legacy = workflow(&authority, &[&["p", &agent]]);
        assert!(!from_owner_or_workflow(&legacy, &owner, &agent, Some(&relay)).await);
        // A workflow the owner made needs no profile.
        let mut own = prompt.clone();
        own.tags[1][1] = owner.clone();
        assert!(from_owner_or_workflow(&own, &owner, &agent, Some(&relay)).await);
    }
}
