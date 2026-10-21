//! Display names for people and agents (`identity-names/policy.ts`,
//! `identity-names/directory.ts`): public profile names, with the viewer's
//! own agents named from their library, and namesakes inside one channel
//! told apart by owner, "(agent)", or an npub suffix.
//!
//! Names are display only; they never select recipients or grant control.

use super::keys::npub;
use buzz_relay_client::{Profiles, RelayOrigin};
use gpui_kit::App;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// One identity as the naming policy sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NamingIdentity {
    pub pubkey: String,
    pub name: String,
    pub is_agent: bool,
    pub owner: Option<String>,
}

/// A name a view shows for a key (`displayFacts`): its `is_agent`, when
/// left out, is what Buzz knows of the key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayFact {
    pub pubkey: String,
    pub name: String,
    pub is_agent: Option<bool>,
    pub owner: Option<String>,
}

/// A resolved label and the npub suffix that made it unique, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub qualifier: Option<String>,
}

/// `resolveIdentityNames`: every displayed alias compared across distinct
/// keys. A namesake keeps its name when it outranks the others (the viewer,
/// then people, then the viewer's agents, then other agents); the rest are
/// qualified by owner ("Ann’s Helper") or "(agent)", then by a growing npub
/// suffix.
pub fn resolve(
    identities: &[NamingIdentity],
    viewer: Option<&str>,
    candidates: Option<&[String]>,
) -> HashMap<String, Label> {
    let viewer = viewer.map(str::to_ascii_lowercase);
    let mut unique: HashMap<String, NamingIdentity> = HashMap::new();
    for identity in identities {
        let mut row = identity.clone();
        row.owner = row.owner.map(|o| o.to_ascii_lowercase());
        unique.insert(identity.pubkey.to_ascii_lowercase(), row);
    }
    let selected: Option<HashSet<String>> =
        candidates.map(|keys| keys.iter().map(|k| k.to_ascii_lowercase()).collect());
    // Repeated configurations of one key are not competing identities: the
    // last alias for a (key, name) pair wins, in first-seen order.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut aliases: HashMap<(String, String), NamingIdentity> = HashMap::new();
    for identity in identities {
        let key = (
            identity.pubkey.to_ascii_lowercase(),
            js_trim(&identity.name).to_string(),
        );
        let mut row = identity.clone();
        row.owner = row.owner.map(|o| o.to_ascii_lowercase());
        if aliases.insert(key.clone(), row).is_none() {
            order.push(key);
        }
    }
    struct Row {
        key: String,
        identity: NamingIdentity,
        base: String,
        label: String,
        priority: u8,
        mine: bool,
        length: usize,
        suffix: Option<String>,
    }
    let mut rows: Vec<Row> = order
        .into_iter()
        .filter_map(|alias| aliases.remove(&alias))
        .filter(|identity| {
            selected
                .as_ref()
                .is_none_or(|s| s.contains(&identity.pubkey.to_ascii_lowercase()))
        })
        .map(|identity| {
            let key = identity.pubkey.to_ascii_lowercase();
            let mine = viewer
                .as_ref()
                .is_some_and(|v| *v == key || identity.owner.as_ref() == Some(v));
            let priority = match (identity.is_agent, mine) {
                (true, true) => 2,
                (true, false) => 3,
                (false, _) if viewer.as_deref() == Some(key.as_str()) => 0,
                (false, _) => 1,
            };
            let name = js_trim(&identity.name).to_string();
            Row {
                key,
                identity,
                base: name.clone(),
                label: name,
                priority,
                mine,
                length: 0,
                suffix: None,
            }
        })
        .collect();
    loop {
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            match groups.iter_mut().find(|(label, _)| *label == row.label) {
                Some((_, members)) => members.push(index),
                None => groups.push((row.label.clone(), vec![index])),
            }
        }
        let collisions: Vec<Vec<usize>> = groups
            .into_iter()
            .map(|(_, members)| members)
            .filter(|members| {
                let keys: HashSet<&str> = members.iter().map(|&i| rows[i].key.as_str()).collect();
                keys.len() > 1
            })
            .collect();
        if collisions.is_empty() {
            break;
        }
        for group in collisions {
            let best = group.iter().map(|&i| rows[i].priority).min().unwrap_or(0);
            let winners: HashSet<String> = group
                .iter()
                .filter(|&&i| rows[i].priority == best)
                .map(|&i| rows[i].key.clone())
                .collect();
            let changing: Vec<usize> = group
                .iter()
                .copied()
                .filter(|&i| winners.len() != 1 || !winners.contains(&rows[i].key))
                .collect();
            let any_person = group.iter().any(|&i| !rows[i].identity.is_agent);
            let mut qualified = false;
            for &i in &changing {
                let row = &rows[i];
                let owner = row
                    .identity
                    .owner
                    .as_ref()
                    .and_then(|owner| unique.get(owner))
                    .map(|owner| js_trim(&owner.name).to_string());
                let name = js_trim(&row.identity.name);
                let readable = if row.identity.is_agent && row.length == 0 {
                    match owner {
                        Some(owner) if !row.mine => format!("{owner}’s {name}"),
                        _ if any_person => format!("{name} (agent)"),
                        _ => row.base.clone(),
                    }
                } else {
                    row.base.clone()
                };
                if readable != row.base {
                    let row = &mut rows[i];
                    row.base = readable.clone();
                    row.label = readable;
                    qualified = true;
                }
            }
            if qualified {
                continue;
            }
            for &i in &changing {
                let row = &mut rows[i];
                row.length = if row.length == 0 { 4 } else { row.length + 1 };
                let npub = npub(&row.key);
                let count = npub.chars().count();
                let suffix = if row.length <= count {
                    npub.chars().skip(count - row.length).collect()
                } else {
                    format!("{npub} · {}", row.length - count)
                };
                row.label = format!("{} · {suffix}", row.base);
                row.suffix = Some(suffix);
            }
        }
    }
    rows.into_iter()
        .filter(|row| {
            unique
                .get(&row.key)
                .is_some_and(|u| js_trim(&u.name) == js_trim(&row.identity.name))
        })
        .map(|row| {
            (
                row.key,
                Label {
                    name: row.label,
                    qualifier: row.suffix,
                },
            )
        })
        .collect()
}

/// JavaScript's `trim()`: Unicode white space and line ends, plus the
/// byte-order mark, which Rust's `trim` keeps.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// The viewer's agents, as the hosts holding their keys list them
/// (`LocalAgents`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentLibrary {
    /// `(pubkey, name, relay url)` in host order.
    pub agents: Vec<(String, String, String)>,
}

impl AgentLibrary {
    /// The signed-in viewer's Agents in every community. Names never grant
    /// control.
    pub fn from_hosts(cx: &App) -> Self {
        let Some(service) = crate::agents2::Agents2::global(cx) else {
            return Self::default();
        };
        let viewer = match &cx.global::<crate::community::AppState>().identity {
            crate::community::IdentityState::Ready(identity) => Some(identity.pubkey_hex()),
            _ => None,
        };
        let agents = service
            .read(cx)
            .local()
            .into_iter()
            .filter(|agent| viewer.as_ref().is_none_or(|viewer| agent.owner == *viewer))
            .map(|agent| (agent.pubkey.to_ascii_lowercase(), agent.name, agent.relay))
            .collect();
        Self { agents }
    }

    /// Their keys.
    pub fn keys(&self) -> std::collections::HashSet<String> {
        self.agents
            .iter()
            .map(|(pubkey, _, _)| pubkey.clone())
            .collect()
    }
}

/// Names for one view: one channel's members, or the whole community.
pub struct Names {
    labels: HashMap<String, Label>,
    /// Library names for keys outside the candidate scope.
    library: HashMap<String, String>,
    agents: HashSet<String>,
    profiles: Profiles,
    viewer: String,
}

impl Names {
    /// The display name for `pubkey`, or `fallback`.
    pub fn name(&self, pubkey: &str, fallback: &str) -> String {
        let key = pubkey.to_ascii_lowercase();
        if let Some(label) = self.labels.get(&key).filter(|l| !l.name.is_empty()) {
            return label.name.clone();
        }
        if let Some(name) = self.library.get(&key) {
            return name.clone();
        }
        self.profiles
            .get(&key)
            .map(|profile| profile.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Where `pubkey`'s name comes from (`IdentityName.source`): the
    /// naming policy's label, a public profile, or neither when the key
    /// shows.
    pub fn source(&self, pubkey: &str) -> Option<&'static str> {
        let key = pubkey.to_ascii_lowercase();
        if self.labels.get(&key).is_some_and(|l| !l.name.is_empty())
            || self.library.contains_key(&key)
        {
            Some("agent-directory")
        } else if self
            .profiles
            .get(&key)
            .is_some_and(|p| !p.name.is_empty() && !p.name_is_fallback)
        {
            Some("public-profile")
        } else {
            None
        }
    }

    /// The generated key suffix that ends `pubkey`'s label (`Honey · 4prr`),
    /// if a namesake needed one.
    pub fn qualifier(&self, pubkey: &str) -> Option<&str> {
        self.labels
            .get(&pubkey.to_ascii_lowercase())
            .and_then(|label| label.qualifier.as_deref())
    }

    /// `profile?.name ?? pubkey.slice(0, 10)` through the policy.
    pub fn display(&self, pubkey: &str) -> String {
        let fallback: String = pubkey.chars().take(10).collect();
        self.name(pubkey, &fallback)
    }

    /// One of the viewer's own agents (`agentPubkeys`).
    pub fn is_library_agent(&self, pubkey: &str) -> bool {
        self.agents.contains(&pubkey.to_ascii_lowercase())
    }

    /// An agent by its envelope, library or signed profile.
    pub fn is_agent(&self, pubkey: &str) -> bool {
        self.is_library_agent(pubkey) || self.profiles.get(pubkey).is_some_and(|p| p.is_agent)
    }

    pub fn profiles(&self) -> &Profiles {
        &self.profiles
    }

    pub fn viewer(&self) -> &str {
        &self.viewer
    }

    /// Library names of the viewer's agents, which also bind mentions.
    pub fn agent_names(&self) -> impl Iterator<Item = (&str, &str)> {
        self.library.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// `createNameProvider`'s identities: every profile, with the viewer's
/// agents named from a signed agent profile, else their library alias,
/// and same-community host settings winning.
pub fn identities(
    profiles: &Profiles,
    library: &AgentLibrary,
    origin: Option<&RelayOrigin>,
) -> (Vec<NamingIdentity>, HashMap<String, String>) {
    let mut names: HashMap<String, String> = HashMap::new();
    for (key, name, _) in &library.agents {
        if names.contains_key(key) {
            continue;
        }
        let profile = profiles.get(key);
        let profile_name = profile.map(|p| js_trim(&p.name).to_string());
        let signed = profile.is_some_and(|p| p.is_agent && !p.name_is_fallback);
        let chosen = match profile_name.filter(|n| !n.is_empty()) {
            Some(profile_name) if signed => profile_name,
            profile_name => Some(js_trim(name).to_string())
                .filter(|n| !n.is_empty())
                .or(profile_name)
                .unwrap_or_else(|| "Agent".into()),
        };
        names.insert(key.clone(), chosen);
    }
    if let Some(origin) = origin {
        let mut seen = HashSet::new();
        for (key, name, relay) in &library.agents {
            let same = RelayOrigin::parse(relay).is_ok_and(|other| other == *origin);
            if !seen.insert(key.clone()) || !same {
                continue;
            }
            if !js_trim(name).is_empty() {
                names.insert(key.clone(), js_trim(name).to_string());
            }
        }
    }
    let mut identities: Vec<NamingIdentity> = profiles
        .iter()
        .filter(|(key, _)| !names.contains_key(key.as_str()))
        .map(|(key, profile)| NamingIdentity {
            pubkey: key.clone(),
            name: profile.name.clone(),
            is_agent: profile.is_agent,
            owner: profile.owner_pubkey.clone(),
        })
        .collect();
    // Stable order: the policy's result does not depend on it, but ties in
    // label groups are compared in order.
    identities.sort_by(|a, b| a.pubkey.cmp(&b.pubkey));
    let mut agents: Vec<(&String, &String)> = names.iter().collect();
    agents.sort();
    for (key, name) in agents {
        identities.push(NamingIdentity {
            pubkey: key.clone(),
            name: name.clone(),
            is_agent: true,
            owner: profiles.get(key).and_then(|p| p.owner_pubkey.clone()),
        });
    }
    (identities, names)
}

thread_local! {
    static CACHE: RefCell<Vec<(CacheKey, Arc<Names>)>> = const { RefCell::new(Vec::new()) };
    static POLICIES: RefCell<Vec<(&'static str, Policy)>> = const { RefCell::new(Vec::new()) };
    static POLICY_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A naming policy a plugin adds (`identityNames.register`): labels for
/// `identities` as [`resolve`] makes them, or `None` while it has no
/// answer yet, when public names show.
pub type Policy = std::rc::Rc<
    dyn Fn(&[NamingIdentity], Option<&str>, Option<&[String]>) -> Option<HashMap<String, Label>>,
>;

/// The bundled policy's plugin.
const BUNDLED: &str = "buzz.identity-naming";

/// Adds a plugin's naming policy. As in buzz-app, where a second
/// `register` is refused, the first policy on applies: the bundled one,
/// else the plugin policy registered first.
pub fn register_policy(key: &'static str, policy: Policy) {
    // Declared again, it keeps its place: the first on still applies.
    POLICIES.with_borrow_mut(|policies| {
        match policies.iter().position(|(existing, _)| *existing == key) {
            Some(at) => policies[at] = (key, policy),
            None => policies.push((key, policy)),
        }
    });
    policy_changed();
}

pub fn unregister_policy(key: &str) {
    POLICIES.with_borrow_mut(|policies| policies.retain(|(existing, _)| *existing != key));
    policy_changed();
}

/// Names need resolving again: a policy came, went or answered.
pub fn policy_changed() {
    POLICY_GENERATION.set(POLICY_GENERATION.get() + 1);
}

/// The policy that applies: the bundled one, else the first plugin's.
enum Active {
    None,
    Bundled,
    Plugin(&'static str, Policy),
}

fn active() -> Active {
    let plugins: Vec<(&'static str, Policy)> = POLICIES.with_borrow(|policies| {
        policies
            .iter()
            .filter(|(key, _)| crate::plugins::enabled_key(key))
            .cloned()
            .collect()
    });
    let bundled = crate::plugins::enabled(BUNDLED);
    match (bundled, plugins.first()) {
        (true, _) => Active::Bundled,
        (false, Some((key, policy))) => Active::Plugin(key, policy.clone()),
        (false, None) => Active::None,
    }
}

/// `identities` with `facts` in place of their keys' rows, each over
/// what the row knew (`{...identities.get(key), ...fact}`).
fn displayed(identities: Vec<NamingIdentity>, facts: &[DisplayFact]) -> Vec<NamingIdentity> {
    // Every fact counts, a key given twice included, each over its key's
    // own row (`directory.ts`).
    let keys: HashSet<String> = facts
        .iter()
        .map(|fact| fact.pubkey.to_ascii_lowercase())
        .collect();
    let (known, mut identities): (Vec<NamingIdentity>, Vec<NamingIdentity>) = identities
        .into_iter()
        .partition(|row| keys.contains(&row.pubkey));
    for fact in facts {
        let key = fact.pubkey.to_ascii_lowercase();
        let row = known.iter().find(|row| row.pubkey == key);
        identities.push(NamingIdentity {
            pubkey: key,
            name: fact.name.clone(),
            is_agent: fact
                .is_agent
                .unwrap_or_else(|| row.is_some_and(|row| row.is_agent)),
            owner: fact
                .owner
                .clone()
                .or_else(|| row.and_then(|row| row.owner.clone())),
        });
    }
    identities
}

#[derive(PartialEq)]
struct CacheKey {
    profiles: usize,
    library: AgentLibrary,
    viewer: String,
    origin: Option<String>,
    members: Option<Vec<String>>,
    /// The policy on: the bundled one, a plugin's by key, or none.
    naming: Option<&'static str>,
    /// Changes when a plugin policy answers.
    generation: u64,
    facts: Vec<DisplayFact>,
}

/// Names scoped to `members` (a channel's roster), or community-wide.
/// Cached per inputs, so rows share one resolution per render.
pub fn names(
    profiles: &Profiles,
    library: &AgentLibrary,
    viewer: &str,
    origin: Option<&RelayOrigin>,
    members: Option<&[String]>,
) -> Arc<Names> {
    names_with(profiles, library, viewer, origin, members, &[])
}

/// [`names`] with `facts`: names shown in this view only, such as a
/// draft's recipients, that replace those keys' rows for the policy
/// (`displayFacts`). They never select recipients or grant control.
pub fn names_with(
    profiles: &Profiles,
    library: &AgentLibrary,
    viewer: &str,
    origin: Option<&RelayOrigin>,
    members: Option<&[String]>,
    facts: &[DisplayFact],
) -> Arc<Names> {
    let key = CacheKey {
        profiles: Arc::as_ptr(profiles) as *const () as usize,
        library: library.clone(),
        viewer: viewer.to_string(),
        origin: origin.map(|o| o.to_string()),
        members: members.map(|m| {
            let mut m: Vec<String> = m.iter().map(|k| k.to_ascii_lowercase()).collect();
            m.sort();
            m
        }),
        naming: None,
        generation: POLICY_GENERATION.get(),
        facts: facts.to_vec(),
    };
    let active = active();
    let key = CacheKey {
        naming: match &active {
            Active::None => None,
            Active::Bundled => Some(BUNDLED),
            Active::Plugin(key, _) => Some(*key),
        },
        ..key
    };
    if let Some(found) = CACHE.with_borrow(|cache| {
        cache
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, names)| names.clone())
    }) {
        return found;
    }
    // Without exactly one policy on there is none: public profile names
    // only (`bindNames` with no provider).
    let (labels, library_names) = match active {
        Active::None => (HashMap::new(), HashMap::new()),
        Active::Bundled => {
            let (identities, library_names) = identities(profiles, library, origin);
            let identities = displayed(identities, facts);
            let labels = resolve(&identities, Some(viewer), key.members.as_deref());
            (labels, library_names)
        }
        Active::Plugin(_, policy) => {
            let (identities, library_names) = identities(profiles, library, origin);
            let identities = displayed(identities, facts);
            let labels = policy(&identities, Some(viewer), key.members.as_deref());
            (labels.unwrap_or_default(), library_names)
        }
    };
    let names = Arc::new(Names {
        labels,
        library: library_names,
        agents: library.agents.iter().map(|(k, _, _)| k.clone()).collect(),
        profiles: profiles.clone(),
        viewer: viewer.to_ascii_lowercase(),
    });
    CACHE.with_borrow_mut(|cache| {
        if cache.len() >= 8 {
            cache.remove(0);
        }
        cache.push((key, names.clone()));
    });
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn id(pubkey: &str, name: &str, is_agent: bool, owner: Option<&str>) -> NamingIdentity {
        NamingIdentity {
            pubkey: pubkey.into(),
            name: name.into(),
            is_agent,
            owner: owner.map(Into::into),
        }
    }

    fn key(byte: u8) -> String {
        buzz_identity::Identity::ephemeral()
            .public_key()
            .to_hex()
            .chars()
            .take(62)
            .collect::<String>()
            + &format!("{byte:02x}")
    }

    #[test]
    fn people_keep_names_and_agents_are_qualified() {
        let (ann, bot, viewer) = (key(1), key(2), key(3));
        let identities = [
            id(&ann, "Ann", false, None),
            id(&bot, "Helper", true, Some(&ann)),
            id(&viewer, "Helper", false, None),
        ];
        let labels = resolve(&identities, Some(&viewer), None);
        assert_eq!(labels[&viewer].name, "Helper");
        assert_eq!(labels[&bot].name, "Ann’s Helper");
        assert_eq!(labels[&ann].name, "Ann");
    }

    #[test]
    fn agents_without_owner_say_agent_then_suffix() {
        let (person, agent) = (key(4), key(5));
        let labels = resolve(
            &[
                id(&person, "Sam", false, None),
                id(&agent, "Sam", true, None),
            ],
            None,
            None,
        );
        assert_eq!(labels[&person].name, "Sam");
        assert_eq!(labels[&agent].name, "Sam (agent)");
        // Two people with one name both take suffixes.
        let (a, b) = (key(6), key(7));
        let labels = resolve(
            &[id(&a, "Kim", false, None), id(&b, "Kim", false, None)],
            None,
            None,
        );
        let suffix = labels[&a].qualifier.clone().unwrap();
        assert_eq!(suffix.len(), 4);
        assert_eq!(labels[&a].name, format!("Kim · {suffix}"));
        assert!(npub(&a).ends_with(&suffix));
        assert_ne!(labels[&a].name, labels[&b].name);
    }

    #[test]
    fn candidates_limit_the_collision_scope() {
        let (a, b) = (key(8), key(9));
        let identities = [id(&a, "Lee", false, None), id(&b, "Lee", false, None)];
        let labels = resolve(&identities, None, Some(std::slice::from_ref(&a)));
        assert_eq!(labels[&a].name, "Lee");
        assert!(!labels.contains_key(&b));
    }

    /// buzz-app's portable fixtures (`bundled/identity-naming`): every
    /// case's expected labels, compared as a key-indexed map.
    #[test]
    fn conforms_to_the_identity_naming_fixtures() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("identity/identity-names.fixtures.json")).unwrap();
        let cases = fixtures["cases"].as_array().unwrap();
        assert!(cases.len() > 20);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let identities: Vec<NamingIdentity> = case["identities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|identity| NamingIdentity {
                    pubkey: identity["pubkey"].as_str().unwrap().to_owned(),
                    name: identity["name"].as_str().unwrap_or_default().to_owned(),
                    is_agent: identity["isAgent"].as_bool().unwrap_or(false),
                    owner: identity["ownerPubkey"].as_str().map(str::to_owned),
                })
                .collect();
            let candidates: Option<Vec<String>> = case["candidates"].as_array().map(|keys| {
                keys.iter()
                    .map(|key| key.as_str().unwrap().to_owned())
                    .collect()
            });
            let labels = resolve(&identities, case["viewer"].as_str(), candidates.as_deref());
            let expected = case["expected"].as_object().unwrap();
            for (key, want) in expected {
                let got = labels
                    .get(&key.to_ascii_lowercase())
                    .unwrap_or_else(|| panic!("{name}: no label for {key}"));
                assert_eq!(got.name, want["name"].as_str().unwrap(), "{name}: {key}");
                assert_eq!(
                    got.qualifier.as_deref(),
                    want["qualifier"].as_str(),
                    "{name}: {key}"
                );
            }
            assert_eq!(labels.len(), expected.len(), "{name}: extra labels");
        }
    }
}
