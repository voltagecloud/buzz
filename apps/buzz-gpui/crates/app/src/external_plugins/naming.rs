//! A naming policy a plugin adds (`identityNames.register`): the plugin
//! labels identities with `names/resolve`. Names are resolved while the
//! app paints and the plugin answers later, so public names show until an
//! answer for the same identities is in; the answer then resolves names
//! again.

use super::intern;
use super::process::Requester;
use crate::ui::identity::{self, Label, NamingIdentity};
use gpui_kit::*;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

/// Most identities one request carries.
const MAX_IDENTITIES: usize = 5_000;
/// Answers kept, oldest dropped first.
const MAX_ANSWERS: usize = 16;
/// How long a failed request waits before it is asked again.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

type Labels = HashMap<String, Label>;
type Entry = (&'static str, u64);

enum Answer {
    Pending,
    /// The request failed or timed out at this time.
    Failed(std::time::Instant),
    Ready(Rc<Labels>),
}

#[derive(Default)]
struct Answers {
    entries: HashMap<Entry, Answer>,
    order: VecDeque<Entry>,
}

/// Where answers come back to the app's thread.
type Inbox = futures::channel::mpsc::UnboundedSender<(Entry, Option<Labels>)>;

thread_local! {
    static ANSWERS: RefCell<Answers> = RefCell::default();
    static INBOX: RefCell<Option<Inbox>> = RefCell::default();
}

pub fn init(cx: &mut App) {
    let (sender, mut answers) = futures::channel::mpsc::unbounded();
    INBOX.with_borrow_mut(|inbox| *inbox = Some(sender));
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some((entry, labels)) = answers.next().await {
            let answered = labels.is_some();
            ANSWERS.with_borrow_mut(|answers| {
                if let Some(answer) = answers.entries.get_mut(&entry) {
                    *answer = match labels {
                        Some(labels) => Answer::Ready(Rc::new(labels)),
                        None => Answer::Failed(std::time::Instant::now()),
                    };
                }
            });
            if !answered {
                // Names are resolved again once it may be asked again.
                let retry = cx.background_executor().timer(RETRY_AFTER);
                cx.spawn(async move |cx| {
                    retry.await;
                    identity::policy_changed();
                    cx.update(|cx| cx.refresh_windows());
                })
                .detach();
                continue;
            }
            identity::policy_changed();
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
}

/// Labels from a `names/resolve` answer: `{"labels": {pubkey: {"name",
/// "qualifier"}}}`, keeping only the keys asked about.
fn labels(value: &Value, asked: &[NamingIdentity]) -> Option<Labels> {
    let given = value.get("labels")?.as_object()?;
    let mut labels = Labels::new();
    for identity in asked {
        let key = identity.pubkey.to_ascii_lowercase();
        let Some(label) = given.get(&key).or_else(|| given.get(&identity.pubkey)) else {
            continue;
        };
        let Some(name) = label.get("name").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        labels.insert(
            key,
            Label {
                name: name.chars().take(128).collect(),
                qualifier: label
                    .get("qualifier")
                    .and_then(Value::as_str)
                    .map(|qualifier| qualifier.chars().take(64).collect()),
            },
        );
    }
    Some(labels)
}

/// The policy for plugin `plugin`'s `id`.
fn policy(key: &'static str, requester: Requester) -> identity::Policy {
    Rc::new(move |identities, viewer, candidates| {
        let identities = &identities[..identities.len().min(MAX_IDENTITIES)];
        let mut hasher = DefaultHasher::new();
        for identity in identities {
            (
                &identity.pubkey,
                &identity.name,
                identity.is_agent,
                &identity.owner,
            )
                .hash(&mut hasher);
        }
        (viewer, candidates).hash(&mut hasher);
        let entry = (key, hasher.finish());
        // A failed request is asked again 30 seconds later.
        let known = ANSWERS.with_borrow(|answers| match answers.entries.get(&entry) {
            Some(Answer::Ready(labels)) => Some(Some((**labels).clone())),
            Some(Answer::Pending) => Some(None),
            Some(Answer::Failed(at)) if at.elapsed() < RETRY_AFTER => Some(None),
            _ => None,
        });
        if let Some(answer) = known {
            return answer;
        }
        ANSWERS.with_borrow_mut(|answers| {
            if answers.entries.insert(entry, Answer::Pending).is_some() {
                return;
            }
            answers.order.push_back(entry);
            while answers.order.len() > MAX_ANSWERS {
                if let Some(old) = answers.order.pop_front() {
                    answers.entries.remove(&old);
                }
            }
        });
        let inbox = INBOX.with_borrow(Clone::clone)?;
        let asked: Vec<NamingIdentity> = identities.to_vec();
        let params = json!({
            "policy": key.split_once('/').map_or(key, |(_, id)| id),
            "identities": asked
                .iter()
                .map(|identity| json!({
                    "pubkey": identity.pubkey,
                    "name": identity.name,
                    "isAgent": identity.is_agent,
                    "owner": identity.owner,
                }))
                .collect::<Vec<_>>(),
            "viewer": viewer,
            "candidates": candidates,
        });
        let request =
            requester.request("names/resolve", params, std::time::Duration::from_secs(10));
        requester.runtime().spawn(async move {
            let answer = match request.await {
                Ok(Ok(value)) => labels(&value, &asked),
                _ => None,
            };
            let _ = inbox.unbounded_send((entry, answer));
        });
        None
    })
}

/// Registers plugin `plugin`'s naming policy `id`; its key, for taking it
/// away.
pub fn register(plugin: &str, id: &str, requester: Requester) -> &'static str {
    let key = intern(format!("{plugin}/{id}"));
    identity::register_policy(key, policy(key, requester));
    key
}

/// Drops the answers of a plugin that restarted.
pub fn forget(plugin: &str) {
    let prefix = format!("{plugin}/");
    ANSWERS.with_borrow_mut(|answers| {
        answers
            .entries
            .retain(|(key, _), _| !key.starts_with(&prefix));
        answers.order.retain(|(key, _)| !key.starts_with(&prefix));
    });
    identity::policy_changed();
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn answers_keep_only_named_keys_asked_about() {
        let asked = vec![
            NamingIdentity {
                pubkey: "AA".into(),
                name: "Ann".into(),
                ..NamingIdentity::default()
            },
            NamingIdentity {
                pubkey: "bb".into(),
                name: "Bo".into(),
                ..NamingIdentity::default()
            },
        ];
        let labels = labels(
            &json!({ "labels": {
                "aa": { "name": " Ann (team) ", "qualifier": "4prr" },
                "bb": { "name": "" },
                "cc": { "name": "Stranger" },
            } }),
            &asked,
        )
        .unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels["aa"].name, "Ann (team)");
        assert_eq!(labels["aa"].qualifier.as_deref(), Some("4prr"));
        assert!(super::labels(&json!(null), &asked).is_none());
    }
}
