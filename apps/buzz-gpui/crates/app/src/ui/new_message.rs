//! "New message" (`NewMessage.tsx`, `RecipientPicker.tsx`): a page of the
//! Messages area. Pick one to eight people in the To field, then either
//! open the DM (or group DM) with exactly them, or write the first message
//! in the composer below and send it there. The relay reuses the
//! conversation for the same people and unhides it.
//!
//! The recipients and the draft are saved as they change. A sent first
//! message carries its recipients and text as outbox recovery until its
//! delivery is confirmed, so a restart or another page can confirm it with
//! "Retry send"; it can be edited only once it failed.

use super::composer::mention_rules::{extends_name, normalize_name};
use super::composer::poof;
use super::composer::{Composer, ComposerEvent, Submission};
use super::modal::{Leading, OpenConversation, row};
use crate::community::{AppState, Community};
use buzz_relay_client::members::Person;
use buzz_relay_client::outbox::Recovery;
use buzz_relay_client::{Delivery, EventId, Mentions, PeoplePage, Profile, kinds};
use buzz_ui::{ActiveTheme, Avatar, AvatarSize, Button, Icon, IconName, Surface, TypeStyled};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::rc::Rc;
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(150);
/// Other people a DM can hold (`direct-message-policy.ts`).
const MAX_RECIPIENTS: usize = 8;
/// The composer's draft, by a channel id no conversation has.
const DRAFT_CHANNEL: &str = "direct-message:new-draft";
const RECIPIENTS: &str = "direct-message:recipients";
/// The outbox recovery key of a first message.
const RECOVERY: &str = "direct-message:new";
const AGENT_UNAVAILABLE: &str =
    "A selected agent is no longer available. Remove it or wait for agent controls to reconnect.";

/// Opens a started conversation at its first message.
pub type OnStarted = Rc<dyn Fn(String, EventId, &mut Window, &mut App)>;

/// A chosen person, as saved: identity and a bounded label only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Recipient {
    pubkey: String,
    name: String,
    #[serde(
        rename = "isAgent",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    is_agent: bool,
    #[serde(skip)]
    picture: Option<String>,
}

impl Recipient {
    fn of(person: &Person) -> Self {
        Self {
            pubkey: person.pubkey.clone(),
            name: person.profile.name.chars().take(500).collect(),
            is_agent: person.profile.is_agent,
            picture: person.profile.picture.clone(),
        }
    }
}

/// `validRecipients`: unique, well-formed, not the viewer, at most eight.
fn valid_recipients(saved: &serde_json::Value, viewer: &str) -> Vec<Recipient> {
    let mut people: Vec<Recipient> = Vec::new();
    for value in saved.as_array().into_iter().flatten() {
        let Ok(mut person) = serde_json::from_value::<Recipient>(value.clone()) else {
            continue;
        };
        let hex = person.pubkey.len() == 64
            && person
                .pubkey
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !hex || person.pubkey == viewer {
            continue;
        }
        person.name = person.name.chars().take(500).collect();
        match people.iter_mut().find(|p| p.pubkey == person.pubkey) {
            Some(existing) => *existing = person,
            None => people.push(person),
        }
    }
    people.truncate(MAX_RECIPIENTS);
    people
}

/// The sorted keys of the people, as the pending message records them.
fn key_for(people: &[Recipient]) -> String {
    let mut keys: Vec<&str> = people.iter().map(|p| p.pubkey.as_str()).collect();
    keys.sort_unstable();
    keys.join(":")
}

/// The first message's text and the people its chips notify.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Draft {
    text: String,
    recipients: Vec<String>,
}

/// A first message whose delivery is not confirmed yet.
#[derive(Clone, Debug)]
struct Pending {
    id: EventId,
    channel: String,
    recipients: String,
    draft: Draft,
    people: Vec<Recipient>,
    failed: bool,
}

/// `recovered`: the outbox's first message with its recovery.
fn recovered(
    ops: &[std::sync::Arc<buzz_relay_client::outbox::Operation>],
    viewer: &str,
) -> Result<Option<Pending>, String> {
    let Some(op) = ops
        .iter()
        .find(|op| op.recovery.as_ref().is_some_and(|r| r.key == RECOVERY))
    else {
        return Ok(None);
    };
    let invalid = || "The saved new message could not be restored.".to_string();
    let value: serde_json::Value =
        serde_json::from_str(&op.recovery.as_ref().expect("found by recovery").value)
            .map_err(|_| invalid())?;
    let people = valid_recipients(&value["people"], viewer);
    let channel = op.channel().map(str::to_owned);
    let (Some(channel), false, true) = (channel, people.is_empty(), op.kind() == kinds::CHAT)
    else {
        return Err(invalid());
    };
    Ok(Some(Pending {
        id: op.id(),
        channel,
        recipients: key_for(&people),
        draft: serde_json::from_value(value["draft"].clone()).unwrap_or_default(),
        people,
        failed: op.delivery == Delivery::Failed,
    }))
}

/// What the page is doing.
#[derive(Clone, Copy, PartialEq)]
enum Busy {
    Opening,
    Sending,
}

/// The people list for one query, page by page (`usePeople`).
struct Found {
    query: String,
    people: Vec<Person>,
    pages: usize,
    more: bool,
    error: bool,
}

pub struct NewMessagePage {
    community: Entity<Community>,
    input: Entity<InputState>,
    composer: Entity<Composer>,
    selected: Vec<Recipient>,
    found: Option<Found>,
    loading: Option<Task<()>>,
    highlighted: usize,
    /// The people list is open under the To field.
    open: bool,
    /// The saved message was read back.
    ready: bool,
    busy: Option<Busy>,
    error: Option<SharedString>,
    pending: Option<Pending>,
    /// The conversation opened for these recipients, reused by Send.
    prepared: Option<(String, String)>,
    bursts: Vec<poof::Burst>,
    people_scroll: ScrollHandle,
    on_open: OpenConversation,
    on_started: OnStarted,
    _outbox: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl NewMessagePage {
    pub fn new(
        community: Entity<Community>,
        on_open: OpenConversation,
        on_started: OnStarted,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find a person…"));
        input.update(cx, |state, cx| state.focus(window, cx));
        let composer = cx.new(|cx| {
            Composer::new(
                community.clone(),
                DRAFT_CHANNEL.into(),
                None,
                "".into(),
                window,
                cx,
            )
        });
        composer.update(cx, |composer, _| composer.without_attachments());
        let subscriptions =
            vec![
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Change => {
                            this.highlighted = 0;
                            this.open = true;
                            this.search(window, cx);
                        }
                        InputEvent::Focus => {
                            this.open = true;
                            cx.notify();
                        }
                        _ => {}
                    }
                }),
                cx.subscribe_in(
                    &composer,
                    window,
                    |this, _, event: &ComposerEvent, window, cx| {
                        if let ComposerEvent::Submit(submission) = event {
                            this.submit(submission.clone(), window, cx);
                        }
                    },
                ),
                cx.observe(&community, |_, _, cx| cx.notify()),
            ];
        let viewer = community.read(cx).session.viewer().to_string();
        let selected = cx
            .global::<AppState>()
            .store
            .view_state(&community.read(cx).partition(), RECIPIENTS)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .map(|saved| valid_recipients(&saved, &viewer))
            .unwrap_or_default();
        let mut page = Self {
            community,
            input,
            composer,
            selected,
            found: None,
            loading: None,
            highlighted: 0,
            open: true,
            ready: false,
            busy: None,
            error: None,
            pending: None,
            prepared: None,
            bursts: Vec::new(),
            people_scroll: ScrollHandle::new(),
            on_open,
            on_started,
            _outbox: None,
            _subscriptions: subscriptions,
        };
        page.restore(window, cx);
        page.search(window, cx);
        page.update_placeholders(window, cx);
        page
    }

    fn viewer(&self, cx: &App) -> String {
        self.community.read(cx).session.viewer().to_string()
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_string()
    }

    /// Reads a first message back from the outbox once it is ready, then
    /// follows its delivery (`useSyncExternalStore(outbox)`).
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.community.read(cx).session.clone();
        let viewer = self.viewer(cx);
        self._outbox = Some(cx.spawn_in(window, async move |this, cx| {
            let Some(outbox) = session.outbox().await else {
                let _ = this.update(cx, |this, cx| {
                    this.ready = true;
                    cx.notify();
                });
                return;
            };
            if outbox.ready().await.is_err() {
                let _ = this.update(cx, |this, cx| {
                    this.error = Some("Could not restore the message.".into());
                    cx.notify();
                });
                return;
            }
            let mut first = true;
            let mut changes = outbox.changes();
            loop {
                let found = recovered(&outbox.visible(), &viewer);
                let keep = this.update_in(cx, |this, window, cx| {
                    match found {
                        Err(error) if first => this.error = Some(error.into()),
                        Err(_) => {}
                        Ok(found) => this.follow(found, first, window, cx),
                    }
                    if first {
                        this.ready = true;
                    }
                    this.lock(cx);
                    cx.notify();
                });
                if keep.is_err() || changes.changed().await.is_err() {
                    return;
                }
                first = false;
            }
        }));
    }

    /// The outbox's first message changed. At first read it restores the
    /// recipients and text (a failed one from what was saved since); once
    /// another page confirmed it, the page starts over empty.
    fn follow(
        &mut self,
        found: Option<Pending>,
        first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (&found, &self.pending) {
            (Some(pending), _) if first => {
                if !pending.failed {
                    self.selected = pending.people.clone();
                    let text = pending.draft.text.clone();
                    self.composer.update(cx, |composer, cx| {
                        composer
                            .input()
                            .update(cx, |state, cx| state.set_value(text, window, cx))
                    });
                } else if self.selected.is_empty() {
                    self.selected = pending.people.clone();
                }
                self.update_placeholders(window, cx);
            }
            (None, Some(_)) if self.busy.is_none() => {
                self.selected.clear();
                self.composer.update(cx, |composer, cx| {
                    composer
                        .input()
                        .update(cx, |state, cx| state.set_value("", window, cx))
                });
                self.update_placeholders(window, cx);
            }
            _ => {}
        }
        self.pending = found;
    }

    /// `locked`: nothing changes before the saved message is read, while
    /// busy, or while an earlier message awaits confirmation.
    fn locked(&self) -> bool {
        !self.ready
            || self.busy.is_some()
            || self.pending.as_ref().is_some_and(|pending| !pending.failed)
    }

    fn lock(&mut self, cx: &mut Context<Self>) {
        let can_send = !self.locked() && !self.selected.is_empty() && self.available(cx);
        self.composer
            .update(cx, |composer, cx| composer.set_can_send(can_send, cx));
    }

    /// `directMessages.available`: a live, writable connection.
    fn available(&self, cx: &App) -> bool {
        let snapshot = &self.community.read(cx).snapshot;
        snapshot.status == buzz_relay_client::SessionStatus::Ready && !snapshot.cached
    }

    fn search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let session = self.community.read(cx).session.clone();
        self.found = None;
        self.loading = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let page = session.people(&query, 1).await;
            let _ = this.update(cx, |this, cx| {
                this.loading = None;
                this.found = Some(match page {
                    Ok(page) => Found {
                        query,
                        people: page.people,
                        pages: 1,
                        more: page.more,
                        error: false,
                    },
                    Err(_) => Found {
                        query,
                        people: Vec::new(),
                        pages: 0,
                        more: true,
                        error: true,
                    },
                });
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// "Load more people", or "Retry loading people" after a failed page.
    fn load_more(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = &self.found else {
            return;
        };
        if self.loading.is_some() || !found.more {
            return;
        }
        let (query, page) = (found.query.clone(), found.pages + 1);
        let session = self.community.read(cx).session.clone();
        self.loading = Some(cx.spawn_in(window, async move |this, cx| {
            let next: Result<PeoplePage, String> = session.people(&query, page).await;
            let _ = this.update(cx, |this, cx| {
                this.loading = None;
                let Some(found) = this.found.as_mut().filter(|f| f.query == query) else {
                    return;
                };
                match next {
                    Ok(next) => {
                        for person in next.people {
                            if !found.people.iter().any(|p| p.pubkey == person.pubkey) {
                                found.people.push(person);
                            }
                        }
                        found.pages = page;
                        found.more = next.more;
                        found.error = false;
                    }
                    Err(_) => found.error = true,
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The viewer's agents this computer controls (`agent.managed`).
    fn controlled(cx: &App) -> Vec<(String, String)> {
        super::identity::AgentLibrary::from_hosts(cx)
            .agents
            .into_iter()
            .map(|(pubkey, name, _)| (pubkey, name))
            .collect()
    }

    /// The people to offer: the directory's, plus controlled agents whose
    /// own name matches, never the viewer, an agent this computer does not
    /// control, or someone already chosen.
    fn candidates(&self, cx: &App) -> Vec<Person> {
        let query = self.query(cx);
        let Some(found) = self.found.as_ref().filter(|found| found.query == query) else {
            return Vec::new();
        };
        let viewer = self.viewer(cx);
        let controlled = Self::controlled(cx);
        let profiles = &self.community.read(cx).snapshot.profiles;
        let mut people = found.people.clone();
        if !query.is_empty() {
            for (pubkey, name) in &controlled {
                if name.is_empty()
                    || people.iter().any(|p| p.pubkey == *pubkey)
                    || buzz_relay_client::members::match_tier(name, &query).is_none()
                {
                    continue;
                }
                let mut profile = profiles
                    .get(pubkey)
                    .map(|profile| (**profile).clone())
                    .unwrap_or_else(|| Profile {
                        name: name.clone(),
                        ..Profile::fallback(pubkey)
                    });
                profile.is_agent = true;
                people.push(Person {
                    pubkey: pubkey.clone(),
                    profile,
                });
            }
        }
        people
            .into_iter()
            .filter(|person| person.pubkey != viewer)
            .filter(|person| {
                !person.profile.is_agent || controlled.iter().any(|(key, _)| *key == person.pubkey)
            })
            .filter(|person| !self.selected.iter().any(|s| s.pubkey == person.pubkey))
            .collect()
    }

    /// `exactName`: Space completes only one exact name, and only while no
    /// longer name continues it ("Avery" waits while "Avery Chen" exists);
    /// a list still loading, failed or with more pages waits too.
    fn exact_recipient(&self, cx: &App) -> Option<Person> {
        let found = self.found.as_ref()?;
        if self.loading.is_some() || found.error || found.more {
            return None;
        }
        let query = self.query(cx);
        let needle = normalize_name(&query);
        if needle.is_empty() {
            return None;
        }
        let controlled = Self::controlled(cx);
        let candidates = self.candidates(cx);
        let names = |person: &Person| -> Vec<String> {
            let mut names = vec![person.profile.name.clone()];
            if let Some((_, own)) = controlled.iter().find(|(key, _)| *key == person.pubkey)
                && !own.is_empty()
                && *own != person.profile.name
            {
                names.push(own.clone());
            }
            names
        };
        if candidates
            .iter()
            .any(|person| names(person).iter().any(|name| extends_name(name, &query)))
        {
            return None;
        }
        let mut exact = candidates.into_iter().filter(|person| {
            names(person)
                .iter()
                .any(|name| normalize_name(name) == needle)
        });
        match (exact.next(), exact.next()) {
            (Some(person), None) => Some(person),
            _ => None,
        }
    }

    fn save_recipients(&self, cx: &App) {
        let json = serde_json::to_string(&self.selected).expect("recipients serialize");
        let saved = cx.global::<AppState>().store.set_view_state(
            &self.community.read(cx).partition(),
            RECIPIENTS,
            Some(&json),
        );
        if let Err(error) = saved {
            tracing::warn!(%error, "could not save the new message's recipients");
        }
    }

    fn clear_recipients(&self, cx: &App) {
        let _ = cx.global::<AppState>().store.set_view_state(
            &self.community.read(cx).partition(),
            RECIPIENTS,
            None,
        );
    }

    /// `change`: a new set of people, saved for the next visit.
    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prepared = None;
        self.error = None;
        self.save_recipients(cx);
        self.update_placeholders(window, cx);
        self.lock(cx);
        cx.notify();
    }

    fn choose(&mut self, person: Person, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() || self.selected.len() >= MAX_RECIPIENTS {
            return;
        }
        self.selected.push(Recipient::of(&person));
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.highlighted = 0;
        self.open = true;
        self.changed(window, cx);
        self.search(window, cx);
    }

    /// Removes a chip with buzz-app's poof and plop at the chip, then
    /// returns focus to the To field (`useChipRemoval`).
    fn remove(
        &mut self,
        pubkey: &str,
        at: Option<Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.locked() || !self.selected.iter().any(|p| p.pubkey == pubkey) {
            return;
        }
        if let Some(at) = at {
            self.bursts.retain(|burst| !burst.done());
            self.bursts.push(poof::Burst::new(at, px(22.)));
        }
        poof::plop();
        self.selected.retain(|p| p.pubkey != pubkey);
        self.input.update(cx, |state, cx| state.focus(window, cx));
        self.open = true;
        self.changed(window, cx);
    }

    /// The To field's placeholder and the composer's label.
    fn update_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = if self.selected.len() >= MAX_RECIPIENTS {
            format!("{MAX_RECIPIENTS} people selected")
        } else if self.selected.is_empty() {
            "Find a person…".to_owned()
        } else {
            "Add another person…".to_owned()
        };
        self.input.update(cx, |state, cx| {
            state.set_placeholder(placeholder, window, cx);
        });
        let names: Vec<&str> = self.selected.iter().map(|p| p.name.as_str()).collect();
        let label = if names.is_empty() {
            String::new()
        } else {
            format!("Message {}", names.join(", "))
        };
        self.composer.update(cx, |composer, cx| {
            composer.set_placeholder(label.into(), window, cx)
        });
    }

    /// `validateAgents`: every chosen agent must still be controlled here.
    fn validate_agents(&self, cx: &App) -> Result<(), String> {
        let controlled = Self::controlled(cx);
        let profiles = &self.community.read(cx).snapshot.profiles;
        let unavailable = self.selected.iter().any(|person| {
            (person.is_agent || profiles.get(&person.pubkey).is_some_and(|p| p.is_agent))
                && !controlled.iter().any(|(key, _)| *key == person.pubkey)
        });
        if unavailable {
            Err(AGENT_UNAVAILABLE.into())
        } else {
            Ok(())
        }
    }

    /// "Open conversation": the DM with exactly these people.
    fn open_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() || !self.ready || self.selected.is_empty() || !self.available(cx) {
            return;
        }
        if let Err(error) = self.validate_agents(cx) {
            self.error = Some(error.into());
            cx.notify();
            return;
        }
        let pubkeys: Vec<String> = self.selected.iter().map(|p| p.pubkey.clone()).collect();
        let session = self.community.read(cx).session.clone();
        self.busy = Some(Busy::Opening);
        self.error = None;
        self.lock(cx);
        cx.spawn_in(window, async move |this, cx| {
            let opened = session.open_dm(pubkeys).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match opened {
                    Ok(id) => {
                        this.clear_recipients(cx);
                        (this.on_open)(id, window, cx);
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                this.lock(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn submit(&mut self, submission: Submission, window: &mut Window, cx: &mut Context<Self>) {
        let draft = Draft {
            text: submission.content.clone(),
            recipients: submission.mentions.recipients.clone(),
        };
        self.send(draft, window, cx);
    }

    /// "Retry send": confirms the earlier message as it was.
    fn retry_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pending) = &self.pending {
            let draft = pending.draft.clone();
            self.send(draft, window, cx);
        }
    }

    /// `send`: the first message through the outbox with its recovery, then
    /// its confirmed delivery. An earlier message must be confirmed first,
    /// or failed and replaced.
    fn send(&mut self, draft: Draft, window: &mut Window, cx: &mut Context<Self>) {
        let settle = |this: &mut Self, ok: bool, window: &mut Window, cx: &mut Context<Self>| {
            this.composer.update(cx, |composer, cx| {
                if ok {
                    composer.sent(window, cx)
                } else {
                    composer.failed(cx)
                }
            });
        };
        if self.busy.is_some()
            || !self.ready
            || self.selected.is_empty()
            || draft.text.trim().is_empty()
            || !self.available(cx)
        {
            settle(self, false, window, cx);
            return;
        }
        let key = key_for(&self.selected);
        let mut current = self.pending.clone();
        let mut replace = None;
        let checked = (|| {
            if let Some(earlier) = &current {
                if earlier.failed {
                    self.validate_agents(cx)?;
                }
                if earlier.recipients != key || earlier.draft != draft {
                    if !earlier.failed {
                        return Err(
                            "Retry the earlier message to confirm its delivery before editing."
                                .to_string(),
                        );
                    }
                    replace = Some(earlier.id);
                    current = None;
                }
            }
            if current.is_none() {
                self.validate_agents(cx)?;
            }
            Ok(())
        })();
        if let Err(error) = checked {
            self.error = Some(error.into());
            settle(self, false, window, cx);
            cx.notify();
            return;
        }
        let session = self.community.read(cx).session.clone();
        let people = self.selected.clone();
        let pubkeys: Vec<String> = people.iter().map(|p| p.pubkey.clone()).collect();
        let prepared = self
            .prepared
            .clone()
            .filter(|(prepared, _)| *prepared == key)
            .map(|(_, id)| id);
        self.busy = Some(Busy::Sending);
        self.error = None;
        self.lock(cx);
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<Pending, (String, Option<String>)> = async {
                if let Some(id) = replace {
                    session.dismiss(id).await.map_err(|e| (e, None))?;
                }
                let pending = match current {
                    Some(pending) => pending,
                    None => {
                        let channel = match prepared {
                            Some(channel) => channel,
                            None => session.open_dm(pubkeys).await.map_err(|e| (e, None))?,
                        };
                        let opened = Some(channel.clone());
                        let mentions = Mentions {
                            recipients: draft.recipients.clone(),
                            references: Vec::new(),
                            ..Default::default()
                        };
                        session
                            .send(&channel, &draft.text, mentions, Vec::new(), None)
                            .await
                            .map_err(|e| (e, opened.clone()))?;
                        let outbox = session
                            .outbox()
                            .await
                            .ok_or_else(|| ("Not connected to the relay yet".to_string(), None))?;
                        let id = queued(&outbox, session.viewer(), &channel, &draft.text)
                            .ok_or_else(|| {
                                ("The message could not be confirmed.".to_string(), None)
                            })?;
                        let recovery = Recovery {
                            key: RECOVERY.into(),
                            value: json!({"people": people, "draft": draft}).to_string(),
                        };
                        outbox
                            .recover(&id, recovery)
                            .await
                            .map_err(|e| (e.to_string(), None))?;
                        Pending {
                            id,
                            channel,
                            recipients: key_for(&people),
                            draft: draft.clone(),
                            people: people.clone(),
                            failed: false,
                        }
                    }
                };
                session
                    .dm_delivered(pending.id, &pending.channel)
                    .await
                    .map_err(|e| (e, None))?;
                Ok(pending)
            }
            .await;
            let delivered = match result {
                Ok(pending) => Ok(pending),
                Err((error, opened)) => {
                    let _ = this.update(cx, |this, _| {
                        if let Some(channel) = opened {
                            this.prepared = Some((key_for(&people), channel));
                        }
                    });
                    Err(error)
                }
            };
            let Ok(pending) = delivered else {
                let error = delivered.err().unwrap_or_default();
                let _ = this.update_in(cx, |this, window, cx| {
                    this.busy = None;
                    this.error = Some(error.into());
                    settle(this, false, window, cx);
                    this.lock(cx);
                    cx.notify();
                });
                return;
            };
            // Disposable views go before the durable recovery: a failed
            // clear keeps the message for a confirmation-only retry.
            let _ = this.update_in(cx, |this, window, cx| {
                this.clear_recipients(cx);
                settle(this, true, window, cx);
            });
            let acknowledged = match session.outbox().await {
                Some(outbox) => outbox
                    .acknowledge(&pending.id)
                    .await
                    .map_err(|e| e.to_string()),
                None => Ok(()),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match acknowledged {
                    Ok(()) => {
                        this.pending = None;
                        this.selected.clear();
                        this.update_placeholders(window, cx);
                        (this.on_started)(pending.channel, pending.id, window, cx);
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                this.lock(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn avatar(
        &self,
        pubkey: &str,
        name: &str,
        picture: Option<&str>,
        agent: bool,
        size: f32,
        cx: &App,
    ) -> Avatar {
        let mut avatar = Avatar::new(name.to_string()).size(AvatarSize::Px(size));
        if agent {
            avatar = avatar.squircle();
        }
        let picture = picture.map(str::to_owned).or_else(|| {
            self.community
                .read(cx)
                .snapshot
                .profiles
                .get(pubkey)
                .and_then(|p| p.picture.clone())
        });
        if let Some(picture) = picture {
            let media = self.community.read(cx).session.media().clone();
            avatar = avatar.image(super::media::avatar(&picture, &media));
        }
        avatar
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform {
            return;
        }
        let query = self.query(cx);
        let full = self.selected.len() >= MAX_RECIPIENTS;
        let listed = self.open && !self.locked() && !full;
        let candidates = if listed {
            self.candidates(cx)
        } else {
            Vec::new()
        };
        match keystroke.key.as_str() {
            "backspace" if self.input.read(cx).value().is_empty() => {
                if let Some(last) = self.selected.last().map(|p| p.pubkey.clone()) {
                    self.remove(&last, None, window, cx);
                }
            }
            "escape" => {
                self.open = false;
                cx.notify();
            }
            "enter" if full => {}
            "space" if !keystroke.modifiers.shift && !full => {
                match self.exact_recipient(cx).filter(|_| listed) {
                    Some(person) => self.choose(person, window, cx),
                    None => return,
                }
            }
            "tab" if !keystroke.modifiers.shift && !full => {
                match candidates.get(self.highlighted).cloned() {
                    Some(person) => self.choose(person, window, cx),
                    None => return,
                }
            }
            "down" if !candidates.is_empty() => {
                self.highlighted = (self.highlighted + 1) % candidates.len();
                cx.notify();
            }
            "up" if !candidates.is_empty() => {
                self.highlighted = (self.highlighted + candidates.len() - 1) % candidates.len();
                cx.notify();
            }
            "enter" => {
                if let Some(person) = candidates.get(self.highlighted).cloned()
                    && (!query.is_empty() || listed)
                {
                    self.choose(person, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn people_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        if self.selected.len() >= MAX_RECIPIENTS {
            return div()
                .p(rems(0.75))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("You can message up to 8 people at once.")
                .into_any_element();
        }
        let candidates = self.candidates(cx);
        if !candidates.is_empty() && self.highlighted >= candidates.len() {
            self.highlighted = candidates.len() - 1;
        }
        let query = self.query(cx);
        let found = self.found.as_ref().filter(|found| found.query == query);
        let loading = self.loading.is_some() || found.is_none();
        let error = found.is_some_and(|found| found.error);
        let more = found.is_some_and(|found| found.more);
        // Like the reference's scroll handler: near the end, the next page.
        let offset = -f32::from(self.people_scroll.offset().y);
        let max = f32::from(self.people_scroll.max_offset().y);
        if more && !loading && !error && max > 0. && max - offset < 80. {
            cx.defer_in(window, |this, window, cx| this.load_more(window, cx));
        }
        let mut list = div()
            .id("new-message-people")
            .track_scroll(&self.people_scroll)
            .max_h(px(400.))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        let named: Vec<(String, String)> = candidates
            .iter()
            .map(|p| (p.pubkey.clone(), p.profile.name.clone()))
            .chain(
                self.selected
                    .iter()
                    .map(|p| (p.pubkey.clone(), p.name.clone())),
            )
            .collect();
        let discriminators = discriminators(&named);
        for (index, person) in candidates.iter().enumerate() {
            let chosen = person.clone();
            let detail = [
                discriminators
                    .get(&person.pubkey)
                    .cloned()
                    .unwrap_or_default(),
                if person.profile.is_agent {
                    "Agent".into()
                } else {
                    String::new()
                },
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
            let detail = (!detail.is_empty()).then(|| SharedString::from(detail));
            let avatar = self.avatar(
                &person.pubkey,
                &person.profile.name,
                person.profile.picture.as_deref(),
                person.profile.is_agent,
                32.,
                cx,
            );
            list = list.child(
                row(
                    SharedString::from(format!("person-{}", person.pubkey)),
                    Leading::Element(avatar.into_any_element()),
                    person.profile.name.clone(),
                    detail,
                    index == self.highlighted,
                    cx,
                )
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                .on_click(
                    cx.listener(move |this, _, window, cx| this.choose(chosen.clone(), window, cx)),
                ),
            );
        }
        let text = |text: &'static str| {
            div()
                .p(rems(0.75))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
        };
        if !loading && !error && candidates.is_empty() {
            list = list.child(text("No matching people."));
        }
        if loading && (query.is_empty() || candidates.is_empty()) {
            list = list.child(text("Loading people…"));
        }
        if error {
            list = list.child(text("Could not load people."));
        }
        if !loading && more {
            list = list.child(
                div().p(rems(0.25)).child(
                    Button::new(
                        "people-more",
                        if error {
                            "Retry loading people"
                        } else {
                            "Load more people"
                        },
                    )
                    .surface(Surface::Floating)
                    .on_click(cx.listener(|this, _, window, cx| this.load_more(window, cx))),
                ),
            );
        }
        list.into_any_element()
    }
}

/// Namesakes among the people shown and chosen, told apart by the
/// shortest distinct npub ending (`publicKeyLabels` per name).
fn discriminators(people: &[(String, String)]) -> std::collections::HashMap<String, String> {
    let mut by_name: std::collections::HashMap<&str, Vec<String>> = Default::default();
    for (pubkey, name) in people {
        let keys = by_name.entry(name.as_str()).or_default();
        if !keys.contains(pubkey) {
            keys.push(pubkey.clone());
        }
    }
    by_name
        .into_values()
        .filter(|keys| keys.len() > 1)
        .flat_map(|keys| super::keys::public_key_labels(keys.iter()))
        .collect()
}

/// The outbox's newest message with this text in `channel`, which
/// `Session::send` just queued.
fn queued(
    outbox: &buzz_relay_client::outbox::Outbox,
    viewer: &str,
    channel: &str,
    text: &str,
) -> Option<EventId> {
    outbox
        .visible()
        .iter()
        .filter(|op| {
            op.kind() == kinds::CHAT
                && op.channel() == Some(channel)
                && op.event.content == text
                && op.event.pubkey.to_hex() == viewer
        })
        .max_by_key(|op| op.created_at())
        .map(|op| op.id())
}

impl Render for NewMessagePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let locked = self.locked();
        let available = self.available(cx);
        let failed = self.pending.as_ref().is_some_and(|pending| pending.failed);
        let chips: Vec<_> = self
            .selected
            .clone()
            .into_iter()
            .map(|person| {
                let key = person.pubkey.clone();
                let name = person.name.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.25))
                    .max_w(rems(15.))
                    .pl(px(2.))
                    .pr(rems(0.5))
                    .py(px(2.))
                    .rounded_full()
                    .bg(c.surface_inset)
                    .text_body_sm()
                    .child(
                        div()
                            .id(SharedString::from(format!("remove-{}", person.pubkey)))
                            .group("chip-remove")
                            .relative()
                            .size(rems(1.375))
                            .flex_none()
                            .rounded_full()
                            .cursor_pointer()
                            .aria_label(format!("Remove {name}"))
                            .child(self.avatar(
                                &person.pubkey,
                                &person.name,
                                person.picture.as_deref(),
                                person.is_agent,
                                22.,
                                cx,
                            ))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(c.surface_inverse)
                                    .text_color(c.text_inverse)
                                    .invisible()
                                    .group_hover("chip-remove", |s| s.visible())
                                    .child(Icon::new(IconName::X).size(12.)),
                            )
                            .on_mouse_down(MouseButton::Left, |_, window, _| {
                                window.prevent_default()
                            })
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                let at = (!event.is_keyboard()).then(|| event.position());
                                this.remove(&key, at, window, cx)
                            })),
                    )
                    .child(div().truncate().child(name))
                    .when(person.is_agent, |chip| {
                        chip.child(
                            div()
                                .text_caption()
                                .text_color(c.text_subtle)
                                .child("Agent"),
                        )
                    })
            })
            .collect();
        let listed = self.open && !locked;
        let picker = listed.then(|| {
            deferred(
                div()
                    .absolute()
                    .top_full()
                    .left(rems(1.5))
                    .mt(px(12.))
                    .w(rems(23.75))
                    .p(rems(0.25))
                    .rounded(px(16.))
                    .bg(c.surface_popover)
                    .border_1()
                    .border_color(c.border_standard)
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.open = false;
                        cx.notify();
                    }))
                    .child(self.people_list(window, cx)),
            )
            .with_priority(1)
        });
        let header = div()
            .relative()
            .px(rems(1.5))
            .py(rems(1.))
            .border_b_1()
            .border_color(c.border_standard)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(rems(0.5))
                    .min_h(rems(2.))
                    .capture_key_down(cx.listener(Self::key_down))
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .mr(rems(0.25))
                            .child("To:"),
                    )
                    .children(chips)
                    .child(
                        div().flex_1().min_w(rems(8.75)).child(
                            Input::new(&self.input)
                                .appearance(false)
                                .disabled(locked)
                                .px_0()
                                .py_0()
                                .text_body_sm(),
                        ),
                    ),
            )
            .children(picker);
        let mut feedback = div()
            .flex()
            .flex_col()
            .items_start()
            .gap(rems(0.5))
            .px(rems(1.5));
        if !self.selected.is_empty() && self.pending.is_none() {
            feedback = feedback.child(
                Button::new("new-message-open", "Open conversation")
                    .disabled(locked || !available)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_conversation(window, cx)),
                    ),
            );
        }
        if let Some(busy) = self.busy {
            feedback = feedback.child(div().text_body_sm().child(match busy {
                Busy::Opening => "Opening conversation…",
                Busy::Sending => "Sending message…",
            }));
        }
        if !available {
            feedback = feedback.child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child("Starting direct messages is unavailable on this connection."),
            );
        }
        let retry = |id: &'static str, disabled: bool, cx: &mut Context<Self>| {
            Button::new(id, "Retry send")
                .disabled(disabled)
                .on_click(cx.listener(|this, _, window, cx| this.retry_send(window, cx)))
        };
        if self.pending.is_some() && !failed && self.busy.is_none() && self.error.is_none() {
            feedback = feedback.child(retry("new-message-retry", false, cx));
        }
        if let Some(error) = self.error.clone() {
            feedback = feedback.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(rems(0.5))
                    .child(div().text_body_sm().text_color(c.text_danger).child(error))
                    .when(self.pending.is_some() && !failed, |this| {
                        this.child(retry("new-message-retry-error", self.busy.is_some(), cx))
                    }),
            );
        }
        self.bursts.retain(|burst| !burst.done());
        div()
            .id("new-message")
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .bg(c.surface_panel)
            .aria_label("New message")
            .child(header)
            .child(div().flex_1().min_h_0())
            .child(feedback)
            .child(self.composer.clone())
            .children(poof::overlay(&self.bursts, window, cx))
            .child(poof::preload())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn saved_recipients_are_validated_like_the_reference() {
        let saved = json!([
            {"pubkey": B, "name": "Bob", "isAgent": true},
            {"pubkey": A, "name": "Me"},
            {"pubkey": "nothex", "name": "x"},
            {"pubkey": B, "name": "Bobby"},
            {"pubkey": "c".repeat(64), "name": "c".repeat(600)},
        ]);
        let people = valid_recipients(&saved, A);
        assert_eq!(people.len(), 2);
        assert_eq!(people[0].name, "Bobby");
        assert_eq!(people[1].name.chars().count(), 500);
        let json = serde_json::to_value(&people[0]).unwrap();
        assert_eq!(json, json!({"pubkey": B, "name": "Bobby"}));
    }

    #[test]
    fn recipient_keys_ignore_order() {
        let person = |pubkey: &str| Recipient {
            pubkey: pubkey.into(),
            name: String::new(),
            is_agent: false,
            picture: None,
        };
        assert_eq!(
            key_for(&[person(B), person(A)]),
            key_for(&[person(A), person(B)])
        );
    }
}
