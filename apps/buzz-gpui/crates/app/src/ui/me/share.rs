//! "Share conversation" and "Share session" (`sessions/SessionShare.tsx`,
//! `SessionShare.module.css`).
//!
//! A direct share moves a Me conversation to Messages in place, with its
//! history, Canvas and agents, after saving its channel details and
//! granting the people chosen. Otherwise the session's link is posted in
//! a channel (or a new one), after granting that channel's members or the
//! people chosen access to the session. The session owns a submitted
//! attempt (`Session::begin_share`), so a failure keeps its exact choices
//! for a retry from this view or another, and "Start over" forgets only
//! the choices, never work already done.

use super::MeModel;
use super::dialogs::DialogEvent;
use super::recipients;
use crate::community::{AppState, Community, IdentityState};
use crate::runtime::Runtime;
use crate::ui::form_fields::{self, Bounded, OnDuration, TextKind};
use crate::ui::keys::format_public_key;
use crate::ui::overlay::{self, Align, Dismiss};
use crate::ui::settings::form::note as subtle;
use buzz_relay_client::discovery::session_metadata;
use buzz_relay_client::lifecycle::{
    AttemptStatus, ChannelDetails, DEFAULT_TEMPORARY_TTL, DetailsDraft, DraftErrors,
    canonical_name, draft_errors,
};
use buzz_relay_client::members::can_share_session;
use buzz_relay_client::session::share::{
    self, Audience, Destination, IN_PROGRESS, ShareAttempt, ShareIntent, ShareOptions, Step,
};
use buzz_relay_client::{ChannelSummary, ChannelType, Profiles};
use buzz_ui::{
    ActiveTheme, Button, Dialog, Icon, IconButton, IconButtonSize, IconName, Menu, MenuItem,
    MenuLabel, MenuSeparator, Popover, PopoverPadding, PopoverWidth, Radio, Select, Surface,
    Switch, TypeStyled,
    tokens::{radius, size, space},
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// The people search waits this long after typing.
const DEBOUNCE: Duration = Duration::from_millis(200);
const CHANGED: &str = share::CHANGED;

/// Someone found or chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Person {
    pubkey: String,
    name: String,
    agent: bool,
}

/// Which search field the results belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// "Find people to add to new channel".
    Channel,
    /// "Find people" / "Find people for session access".
    Session,
}

fn npub(pubkey: &str) -> Option<String> {
    use nostr::nips::nip19::ToBech32;
    if pubkey.len() != 64 || !pubkey.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    nostr::prelude::PublicKey::from_hex(&pubkey.to_ascii_lowercase())
        .ok()?
        .to_bech32()
        .ok()
}

/// `publicKeyLabels`: distinct keys share the shortest unambiguous
/// suffix, three characters at least.
fn key_labels<'a>(keys: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let encoded: HashMap<String, String> = keys
        .into_iter()
        .filter_map(|key| Some((key.to_ascii_lowercase(), npub(key)?)))
        .collect();
    let values: Vec<&String> = encoded.values().collect();
    let mut length = 3;
    while length < 58
        && values
            .iter()
            .map(|npub| &npub[npub.len() - length..])
            .collect::<HashSet<_>>()
            .len()
            < values.len()
    {
        length += 1;
    }
    encoded
        .iter()
        .map(|(key, npub)| {
            (
                key.clone(),
                format!("npub…{}", &npub[npub.len() - length..]),
            )
        })
        .collect()
}

/// `normalizedDetails`: the draft with its name canonical.
fn normalized(draft: &DetailsDraft) -> DetailsDraft {
    DetailsDraft {
        name: canonical_name(&draft.name),
        ..draft.clone()
    }
}

/// The draft a loaded channel starts from: a session's marker
/// description is cleared, so the moved conversation is no longer one.
fn initial_draft(base: &ChannelDetails) -> DetailsDraft {
    let mut draft = base.draft.clone();
    if session_metadata(Some(&draft.description)).is_some() {
        draft.description.clear();
    }
    draft
}

/// Where a direct share may move a conversation from (`eligible`).
fn direct_eligible(source: Option<&ChannelSummary>, viewer: &str, me_ready: bool) -> bool {
    source.is_some_and(|source| {
        !source.read_only
            && !source.cached
            && !source.archived
            && matches!(
                source.channel_type,
                Some(ChannelType::Session | ChannelType::Stream)
            )
            && source.members.iter().any(|key| key == viewer)
            && me_ready
    })
}

/// Channels a session link may be posted in.
fn destinations(channels: &[ChannelSummary], viewer: &str) -> Vec<ChannelSummary> {
    channels
        .iter()
        .filter(|c| {
            !c.read_only
                && !c.cached
                && !c.archived
                && matches!(
                    c.channel_type,
                    Some(ChannelType::Stream | ChannelType::Forum)
                )
                && c.members.iter().any(|key| key == viewer)
        })
        .cloned()
        .collect()
}

/// "Retry to …": where a saved attempt continues, and for whom.
fn retry_status(attempt: &ShareAttempt, destinations: &[ChannelSummary]) -> String {
    let name_of = |id: &str| {
        destinations
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
    };
    let intent = &attempt.intent;
    let place = match (attempt.created(), &intent.destination) {
        (Some(created), _) => name_of(&created).unwrap_or_else(|| "the created channel".into()),
        (None, Destination::New) => intent.name.clone().unwrap_or_else(|| "new channel".into()),
        (None, Destination::Channel(id)) => {
            name_of(id).unwrap_or_else(|| "the chosen channel".into())
        }
    };
    let audience = match intent.audience {
        Audience::Everyone => ", with the saved channel audience.".to_owned(),
        Audience::Selected => format!(
            ", with {} selected for session access.",
            intent.session_people.len()
        ),
    };
    let channel = if intent.destination == Destination::New && !intent.channel_people.is_empty() {
        format!(
            " {} chosen for the new channel.",
            intent.channel_people.len()
        )
    } else {
        String::new()
    };
    format!("Retry to {place}{audience}{channel}")
}

/// Bounds of the search field the results hang from.
type Anchor = Rc<Cell<Option<Bounds<Pixels>>>>;

pub struct SessionShare {
    community: Entity<Community>,
    channel: ChannelSummary,
    /// Opened as "Share conversation" (Me always is).
    direct: bool,
    /// The attempt this view shows; a retry never reinterprets it.
    frozen: Option<Arc<ShareAttempt>>,
    // Direct share: the channel's details.
    base: Option<ChannelDetails>,
    details: Option<DetailsDraft>,
    details_loading: bool,
    details_task: Option<Task<()>>,
    name: Entity<InputState>,
    name_bound: Bounded,
    confirm_public: bool,
    // Another channel.
    destination: Option<Destination>,
    picking: bool,
    new_name: Entity<InputState>,
    _new_name_bound: Bounded,
    new_private: bool,
    audience: Audience,
    // People.
    people: Vec<Person>,
    channel_people: Vec<Person>,
    session_search: Entity<InputState>,
    channel_search: Entity<InputState>,
    target: Target,
    results: Vec<Person>,
    searching: bool,
    search_error: Option<String>,
    search_task: Option<Task<()>>,
    results_open: bool,
    show_selected: bool,
    active: Option<String>,
    anchor: Anchor,
    archived: HashSet<String>,
    // Outcome.
    error: Option<String>,
    progress: Option<String>,
    busy: bool,
    copy_status: Option<&'static str>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for SessionShare {}

impl SessionShare {
    pub fn new(
        community: Entity<Community>,
        channel: ChannelSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("release-notes"));
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder("release-notes"));
        let mut search = |cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder("Search by name or public key…"))
        };
        let (session_search, channel_search) = (search(cx), search(cx));
        let name_bound = Bounded::new(&name, TextKind::Name.limit(), window, cx);
        let new_name_bound = Bounded::new(&new_name, TextKind::Name.limit(), window, cx);
        let me = MeModel::global(cx);
        let mut subscriptions = vec![
            cx.observe(&community, |_, _, cx| cx.notify()),
            cx.observe(&me, |_, _, cx| cx.notify()),
            cx.subscribe_in(&name, window, |this, state, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = state.read(cx).value().to_string();
                    if let Some(details) = &mut this.details {
                        details.name = value;
                    }
                    cx.notify();
                }
            }),
            cx.subscribe_in(&new_name, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            }),
        ];
        for (state, target) in [
            (session_search.clone(), Target::Session),
            (channel_search.clone(), Target::Channel),
        ] {
            subscriptions.push(cx.subscribe_in(
                &state,
                window,
                move |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Focus => this.search_focused(target, window, cx),
                    InputEvent::Change => this.search_changed(target, window, cx),
                    _ => {}
                },
            ));
        }
        let frozen = community.read(cx).session.share_attempt(&channel.id);
        let mut this = Self {
            community,
            channel,
            direct: true,
            frozen,
            base: None,
            details: None,
            details_loading: false,
            details_task: None,
            name,
            name_bound,
            confirm_public: false,
            destination: None,
            picking: false,
            new_name,
            _new_name_bound: new_name_bound,
            new_private: false,
            audience: Audience::Selected,
            people: Vec::new(),
            channel_people: Vec::new(),
            session_search,
            channel_search,
            target: Target::Session,
            results: Vec::new(),
            searching: false,
            search_error: None,
            search_task: None,
            results_open: false,
            show_selected: false,
            active: None,
            anchor: Rc::default(),
            archived: HashSet::new(),
            error: None,
            progress: None,
            busy: false,
            copy_status: None,
            _subscriptions: subscriptions,
        };
        this.show_saved(window, cx);
        if this.direct_share(cx) {
            this.load_details(window, cx);
        }
        this.read_archives(cx);
        this
    }

    /// A share is running: the scrim and Escape leave it open.
    pub fn busy(&self) -> bool {
        self.busy
    }

    // ----- state the reference derives -----

    fn session(&self, cx: &App) -> Arc<buzz_relay_client::Session> {
        self.community.read(cx).session.clone()
    }

    fn viewer(&self, cx: &App) -> String {
        self.community.read(cx).session.viewer().to_owned()
    }

    /// `saved`: the frozen attempt, or one another view started since.
    fn saved(&self, cx: &App) -> Option<Arc<ShareAttempt>> {
        self.frozen
            .clone()
            .or_else(|| self.session(cx).share_attempt(&self.channel.id))
    }

    /// A saved attempt keeps its own kind of share.
    fn direct_share(&self, cx: &App) -> bool {
        match self.saved(cx) {
            Some(saved) => saved.intent.is_direct(&self.channel.id),
            None => self.direct,
        }
    }

    /// The source as the sidebar knows it now.
    fn source(&self, cx: &App) -> Option<ChannelSummary> {
        self.community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel.id)
            .cloned()
    }

    fn eligible(&self, cx: &App) -> bool {
        let source = self.source(cx);
        let viewer = self.viewer(cx);
        if self.direct_share(cx) {
            let kit = self.session(cx).me_kit().snapshot();
            direct_eligible(source.as_ref(), &viewer, kit.ready())
        } else {
            source
                .as_ref()
                .is_some_and(|source| can_share_session(source, &viewer))
        }
    }

    fn destinations(&self, cx: &App) -> Vec<ChannelSummary> {
        destinations(&self.community.read(cx).snapshot.channels, &self.viewer(cx))
    }

    /// The details shown: the saved attempt's, else the draft.
    fn shared_details(&self, cx: &App) -> Option<DetailsDraft> {
        self.saved(cx)
            .and_then(|saved| saved.intent.details.clone())
            .or_else(|| self.details.clone())
    }

    fn details_errors(&self, cx: &App) -> Option<DraftErrors> {
        self.shared_details(cx).map(|details| {
            let details = normalized(&details);
            draft_errors(&details.name, &details.description, details.ttl)
        })
    }

    fn details_invalid(&self, cx: &App) -> bool {
        self.direct_share(cx)
            && (!self.base.as_ref().is_some_and(|base| base.can_edit)
                || self.details_loading
                || self.shared_details(cx).is_none()
                || self.details_errors(cx).is_some_and(|errors| errors.any()))
    }

    fn chosen_destination(&self, cx: &App) -> Option<Destination> {
        self.saved(cx)
            .map(|saved| saved.intent.destination.clone())
            .or_else(|| self.destination.clone())
    }

    fn new_channel(&self, cx: &App) -> bool {
        self.chosen_destination(cx) == Some(Destination::New)
    }

    fn chosen_audience(&self, cx: &App) -> Audience {
        self.saved(cx)
            .map(|saved| saved.intent.audience)
            .unwrap_or(self.audience)
    }

    /// `chosen`: the destination channel, created or picked.
    fn chosen(&self, cx: &App) -> Option<ChannelSummary> {
        let id = match self.saved(cx) {
            Some(saved) => saved.target(),
            None => match &self.destination {
                Some(Destination::Channel(id)) => Some(id.clone()),
                _ => None,
            },
        }?;
        self.destinations(cx).into_iter().find(|c| c.id == id)
    }

    fn new_name(&self, cx: &App) -> String {
        canonical_name(&self.new_name.read(cx).value())
    }

    fn query(&self, cx: &App) -> String {
        self.search_input(self.target)
            .read(cx)
            .value()
            .trim()
            .to_owned()
    }

    fn search_input(&self, target: Target) -> &Entity<InputState> {
        match target {
            Target::Channel => &self.channel_search,
            Target::Session => &self.session_search,
        }
    }

    fn selected_people(&self) -> &[Person] {
        match self.target {
            Target::Channel => &self.channel_people,
            Target::Session => &self.people,
        }
    }

    fn known_agents(&self, cx: &App) -> HashSet<String> {
        recipients::rules(&self.community, cx)
            .selectable
            .into_iter()
            .collect()
    }

    fn profiles(&self, cx: &App) -> Profiles {
        self.community.read(cx).snapshot.profiles.clone()
    }

    /// Everyone this view knows by key: chosen or found.
    fn shown(&self) -> Vec<&Person> {
        let mut seen = HashSet::new();
        self.people
            .iter()
            .chain(&self.channel_people)
            .chain(&self.results)
            .filter(|person| seen.insert(person.pubkey.as_str()))
            .collect()
    }

    fn name_of(&self, key: &str, cx: &App) -> String {
        self.profiles(cx)
            .get(key)
            .map(|profile| profile.name.clone())
            .or_else(|| {
                self.shown()
                    .into_iter()
                    .find(|person| person.pubkey == key)
                    .map(|person| person.name.clone())
            })
            .or_else(|| format_public_key(key))
            .unwrap_or_else(|| "Member".into())
    }

    fn known_agent(&self, key: &str, known: &HashSet<String>, cx: &App) -> bool {
        known.contains(key)
            || self.profiles(cx).get(key).is_some_and(|p| p.is_agent)
            || self
                .shown()
                .into_iter()
                .any(|person| person.pubkey == key && person.agent)
    }

    /// Who the search target already admits: the chosen channel or the
    /// session.
    fn target_members(&self, cx: &App) -> Vec<String> {
        match self.target {
            Target::Channel => self.chosen(cx).map(|c| c.members),
            Target::Session => self.source(cx).map(|c| c.members),
        }
        .unwrap_or_default()
    }

    /// Found people who are not agents or already chosen.
    fn candidates(&self, cx: &App) -> Vec<Person> {
        let known = self.known_agents(cx);
        let selected = self.selected_people();
        self.results
            .iter()
            .filter(|person| {
                !self.known_agent(&person.pubkey, &known, cx)
                    && !selected.iter().any(|s| s.pubkey == person.pubkey)
            })
            .cloned()
            .collect()
    }

    /// Candidates that can be chosen now.
    fn available(&self, cx: &App) -> Vec<Person> {
        let members = self.target_members(cx);
        self.candidates(cx)
            .into_iter()
            .filter(|person| {
                !self.archived.contains(&person.pubkey) && !members.contains(&person.pubkey)
            })
            .collect()
    }

    fn popup_visible(&self, cx: &App) -> bool {
        self.results_open && (!self.query(cx).is_empty() || self.show_selected)
    }

    // ----- loading -----

    /// Puts a saved attempt's choices in the fields.
    fn show_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.saved(cx) else {
            return;
        };
        if let Some(details) = &saved.intent.details {
            self.name_bound.set(&self.name, &details.name, window, cx);
        }
        if let Some(name) = &saved.intent.name {
            self.new_name
                .update(cx, |state, cx| state.set_value(name.clone(), window, cx));
        }
    }

    fn load_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session(cx);
        let id = self.channel.id.clone();
        self.details_loading = true;
        self.base = None;
        self.details_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = session.channel_details(&id).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.details_loading = false;
                match result {
                    Ok(base) => {
                        if this.details.is_none() {
                            let draft = initial_draft(&base);
                            if this.saved(cx).is_none() {
                                this.name_bound.set(&this.name, &draft.name, window, cx);
                            }
                            this.details = Some(draft);
                        }
                        this.base = Some(base);
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The community's archived identities, read once
    /// (`identity-archives.ts`): they are shown but cannot be chosen.
    fn read_archives(&mut self, cx: &mut Context<Self>) {
        let IdentityState::Ready(identity) = &cx.global::<AppState>().identity else {
            return;
        };
        let community = self.community.read(cx);
        let Some(authority) = community
            .snapshot
            .relay
            .as_ref()
            .map(|r| r.authority.clone())
        else {
            return;
        };
        let http = buzz_relay_client::RelayHttp::new(community.origin.clone(), identity.clone());
        let read = Runtime::handle(cx).spawn(async move {
            crate::ui::agents::relay::read_archives(&http, &authority, None).await
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(snapshot)) = read.await {
                let _ = this.update(cx, |this, cx| {
                    this.archived = snapshot.archived.into_iter().collect();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ----- people search -----

    fn search_focused(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.target != target {
            let previous = self.search_input(self.target).clone();
            previous.update(cx, |state, cx| state.set_value("", window, cx));
            self.target = target;
            self.show_selected = false;
            self.active = None;
        }
        self.results_open = true;
        self.search(window, cx);
    }

    fn search_changed(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.target != target {
            // Only the focused field has text; clearing the other changes it.
            return;
        }
        self.show_selected = false;
        self.active = None;
        self.results_open = true;
        self.search(window, cx);
    }

    fn search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.query(cx);
        self.search_task = None;
        self.results.clear();
        self.search_error = None;
        if text.is_empty() {
            self.searching = false;
            cx.notify();
            return;
        }
        self.searching = true;
        let session = self.session(cx);
        self.search_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let page = session.search_members(&text, 1).await;
            let _ = this.update(cx, |this, cx| {
                this.searching = false;
                match page {
                    Ok(page) => {
                        this.results = page
                            .people
                            .into_iter()
                            .map(|person| Person {
                                name: person.profile.name.clone(),
                                agent: person.profile.is_agent,
                                pubkey: person.pubkey,
                            })
                            .collect();
                    }
                    Err(error) => this.search_error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn choose(&mut self, person: Person, window: &mut Window, cx: &mut Context<Self>) {
        let known = self.known_agents(cx);
        if self.busy
            || self.saved(cx).is_some()
            || self.known_agent(&person.pubkey, &known, cx)
            || self.archived.contains(&person.pubkey)
            || self.target_members(cx).contains(&person.pubkey)
            || self
                .selected_people()
                .iter()
                .any(|s| s.pubkey == person.pubkey)
        {
            return;
        }
        match self.target {
            Target::Channel => self.channel_people.push(person),
            Target::Session => self.people.push(person),
        }
        self.active = None;
        self.show_selected = false;
        self.results_open = false;
        let input = self.search_input(self.target).clone();
        input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn remove(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.target {
            Target::Channel => self.channel_people.retain(|p| p.pubkey != key),
            Target::Session => self.people.retain(|p| p.pubkey != key),
        }
        let input = self.search_input(self.target).clone();
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// "N selected" and the "· Edit" rows: the chosen people as a list.
    fn show_chosen(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if self.target != target {
            let previous = self.search_input(self.target).clone();
            previous.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.target = target;
        let input = self.search_input(target).clone();
        input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.show_selected = true;
        self.results_open = true;
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.alt || keystroke.modifiers.control || keystroke.modifiers.platform {
            return;
        }
        let visible = self.popup_visible(cx);
        let available = self.available(cx);
        match keystroke.key.as_str() {
            "escape" if visible => self.results_open = false,
            "down" | "up" if !available.is_empty() => {
                self.results_open = true;
                let down = keystroke.key == "down";
                let index = available
                    .iter()
                    .position(|p| Some(&p.pubkey) == self.active.as_ref());
                let next = match index {
                    None if down => 0,
                    None => available.len() - 1,
                    Some(index) if down => (index + 1).min(available.len() - 1),
                    Some(index) => index.saturating_sub(1),
                };
                self.active = Some(available[next].pubkey.clone());
            }
            "enter" if visible => {
                if let Some(person) = available
                    .into_iter()
                    .find(|p| Some(&p.pubkey) == self.active.as_ref())
                {
                    self.choose(person, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    // ----- actions -----

    fn copy(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(share::session_link(
            &self.channel.id,
        )));
        self.copy_status = Some("Link copied");
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        // Escape first closes the results (`onEscape`).
        if self.popup_visible(cx) {
            self.results_open = false;
            cx.notify();
        } else if !self.busy {
            cx.emit(DialogEvent::Close);
        }
    }

    /// Clears the choices (`reset`), keeping the dialog open.
    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.destination = None;
        self.picking = false;
        self.details = None;
        self.base = None;
        self.confirm_public = false;
        self.new_name
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.new_private = false;
        self.audience = if self.direct {
            Audience::Selected
        } else {
            Audience::Everyone
        };
        self.people.clear();
        self.channel_people.clear();
        for input in [self.session_search.clone(), self.channel_search.clone()] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.target = Target::Session;
        self.results_open = false;
        self.show_selected = false;
        self.active = None;
        self.error = None;
        self.progress = None;
        self.frozen = self.session(cx).share_attempt(&self.channel.id);
    }

    /// "Start over": forgets this settled intent only. Access, created
    /// channels, placement and queued operations stay with their owners.
    fn start_over(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.saved(cx) else {
            return;
        };
        if self.busy {
            return;
        }
        let session = self.session(cx);
        if saved.running() {
            self.error = Some(IN_PROGRESS.into());
        } else if !session
            .share_attempt(&self.channel.id)
            .is_some_and(|current| Arc::ptr_eq(&current, &saved))
        {
            self.error = Some(CHANGED.into());
        } else {
            let direct = saved.intent.is_direct(&self.channel.id);
            session.finish_share(&self.channel.id);
            self.reset(window, cx);
            if direct {
                self.load_details(window, cx);
            }
        }
        cx.notify();
    }

    fn primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let public = self.shared_details(cx).is_some_and(|d| !d.private);
        if self.direct_share(cx) && public && self.saved(cx).is_none() && !self.confirm_public {
            self.confirm_public = true;
            cx.notify();
        } else {
            self.submit(window, cx);
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let direct = self.direct_share(cx);
        let new_channel = self.new_channel(cx);
        if !self.eligible(cx)
            || self.busy
            || self.details_invalid(cx)
            || (!direct && !new_channel && self.chosen(cx).is_none())
        {
            return;
        }
        let session = self.session(cx);
        let saved = self.saved(cx);
        let known = self.known_agents(cx);
        if saved.is_none()
            && (self.people.len() > share::MAX_PEOPLE
                || self.channel_people.len() > share::MAX_PEOPLE
                || self
                    .people
                    .iter()
                    .chain(&self.channel_people)
                    .any(|p| self.known_agent(&p.pubkey, &known, cx)))
        {
            self.error =
                Some("Choose at most 100 people in each group; add agents separately.".into());
            cx.notify();
            return;
        }
        // A blocked dialog may outlive another view's successful share:
        // never reinterpret its frozen Retry as a new share.
        if let Some(saved) = &saved
            && !session
                .share_attempt(&self.channel.id)
                .is_some_and(|current| Arc::ptr_eq(&current, saved))
        {
            self.error = Some(CHANGED.into());
            cx.notify();
            return;
        }
        let source = self.channel.id.clone();
        let intent = ShareIntent {
            destination: if direct {
                Destination::Channel(source.clone())
            } else {
                self.destination.clone().unwrap_or(Destination::New)
            },
            audience: if direct {
                Audience::Selected
            } else {
                self.audience
            },
            name: new_channel.then(|| self.new_name(cx)),
            private: new_channel.then_some(self.new_private),
            details: if direct && self.base.is_some() {
                self.details.as_ref().map(normalized)
            } else {
                None
            },
            details_base: if direct { self.base.clone() } else { None },
            channel_people: if new_channel {
                self.channel_people
                    .iter()
                    .map(|p| p.pubkey.clone())
                    .collect()
            } else {
                Vec::new()
            },
            session_people: self.people.iter().map(|p| p.pubkey.clone()).collect(),
        };
        let exact = session.begin_share(&source, intent);
        self.frozen = Some(exact.clone());
        if exact.running() {
            self.error = Some(IN_PROGRESS.into());
            cx.notify();
            return;
        }
        // Search-result agent flags count too.
        let mut known_agents = known;
        known_agents.extend(
            self.shown()
                .into_iter()
                .filter(|p| p.agent)
                .map(|p| p.pubkey.clone()),
        );
        let options = ShareOptions {
            known_agents,
            archived: self.archived.clone(),
        };
        let names: HashMap<String, String> = exact
            .intent
            .channel_people
            .iter()
            .map(|key| (key.clone(), self.name_of(key, cx)))
            .collect();
        self.busy = true;
        self.error = None;
        let (steps, mut receive) = futures::channel::mpsc::unbounded::<Step>();
        // Progress lines follow the run as it reports each step.
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(step) = receive.next().await {
                let label = step.label(|key| names.get(key).cloned().unwrap_or_default());
                if this
                    .update(cx, |this, cx| {
                        this.progress = Some(label);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // Submitted work belongs to the session, not to this dialog.
        cx.spawn_in(window, async move |this, cx| {
            let result = session
                .run_share(&source, &exact, &options, |step| {
                    let _ = steps.unbounded_send(step.clone());
                })
                .await;
            drop(steps);
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.reset(window, cx);
                        cx.emit(if direct {
                            DialogEvent::Shared(source)
                        } else {
                            DialogEvent::Close
                        });
                    }
                    Err(error) => {
                        this.progress = None;
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn check_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session(cx);
        let id = self.channel.id.clone();
        self.busy = true;
        self.error = None;
        cx.spawn_in(window, async move |this, cx| {
            let result = session.check_channel_details(&id).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => this.load_details(window, cx),
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Rendering

/// `.selection`: a users glyph and a line of subtle text.
fn selection(id: &'static str, text: String, cx: &App) -> Stateful<Div> {
    let c = cx.colors();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(space::S2)
        .min_w_0()
        .text_body_sm()
        .text_color(c.text_subtle)
        .child(Icon::new(IconName::Users).size(16.))
        .child(text)
}

impl SessionShare {
    /// The search field (`SearchField` in `.searchAnchor`): a pill on the
    /// subtle affordance, its label hidden.
    fn search_control(
        &self,
        target: Target,
        disabled: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let c = *cx.colors();
        let state = self.search_input(target).clone();
        let has_text = !state.read(cx).value().is_empty();
        let active = self.target == target;
        let anchor = self.anchor.clone();
        let clear = has_text.then(|| {
            let state = state.clone();
            IconButton::new("me-share-search-clear", IconName::X)
                .size(IconButtonSize::Sm)
                .surface(Surface::Floating)
                .on_click(move |_, window, cx| {
                    state.update(cx, |state, cx| {
                        state.set_value("", window, cx);
                        state.focus(window, cx);
                    })
                })
        });
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.key_down(event, window, cx)
            }))
            .when(active, |this| {
                this.child(
                    canvas(
                        move |bounds, _, _| anchor.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S2)
                    .w_full()
                    .min_h(size::CONTROL)
                    .pl(space::S4)
                    .pr(if has_text { rems(0.1875) } else { space::S4 })
                    .rounded(radius::PILL)
                    .border_1()
                    .border_color(
                        if state.read(cx).focus_handle(cx).contains_focused(window, cx) {
                            c.interaction(Surface::Floating).border
                        } else {
                            transparent_black()
                        },
                    )
                    .bg(if disabled {
                        c.affordance_disabled
                    } else {
                        c.affordance_subtle
                    })
                    .text_color(if disabled {
                        c.text_unavailable
                    } else {
                        c.text_standard
                    })
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(16.)
                            .color(c.text_subtle),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&state)
                                .appearance(false)
                                .disabled(disabled)
                                .px_0()
                                .py_0()
                                .h(rems(1.25)),
                        ),
                    )
                    .children(clear.map(|clear| {
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .w(rems(2.))
                            .child(clear)
                    })),
            )
    }

    /// The anchored people list (`popupResults`).
    fn results_popup(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bounds = self.anchor.get()?;
        let c = *cx.colors();
        let saved = self.saved(cx).is_some();
        let members = self.target_members(cx);
        let shown: Vec<String> = self.shown().into_iter().map(|p| p.pubkey.clone()).collect();
        let labels = key_labels(shown.iter().map(String::as_str));
        let label = |key: &str| labels.get(key).cloned().unwrap_or_default();
        let row = |id: SharedString, name: String, detail: String, disabled: bool, active: bool| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .justify_center()
                .w_full()
                .min_h(size::ROW)
                .gap(space::S1)
                .px(space::S3)
                .py(space::S2)
                .rounded(radius::FLOATING_INNER)
                .text_color(if disabled {
                    c.text_unavailable
                } else {
                    c.text_standard
                })
                .when(active && !disabled, |this| {
                    this.bg(c.affordance_floating_hover)
                })
                .when(!disabled, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(c.affordance_floating_hover))
                })
                .when(disabled, |this| this.cursor_not_allowed())
                .child(div().text_body().child(name))
                .child(
                    div()
                        .text_body_sm()
                        .text_color(if disabled {
                            c.text_unavailable
                        } else {
                            c.text_subtle
                        })
                        .child(detail),
                )
        };
        let message = |text: String| {
            div()
                .px(space::S3)
                .py(space::S2)
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
        };
        let mut list = div().flex().flex_col();
        if self.show_selected && self.query(cx).is_empty() {
            for person in self.selected_people() {
                let key = person.pubkey.clone();
                list = list.child(
                    row(
                        format!("me-share-chosen-{key}").into(),
                        format!("{} · Remove", self.name_of(&key, cx)),
                        label(&key),
                        false,
                        false,
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.remove(&key, window, cx)),
                    ),
                );
            }
        } else if self.searching {
            list = list.child(message("Searching…".into()));
        } else if let Some(error) = &self.search_error {
            list = list.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(space::S2)
                    .px(space::S3)
                    .py(space::S2)
                    .text_body_sm()
                    .child(error.clone())
                    .child(
                        Button::new("me-share-search-retry", "Retry")
                            .size(buzz_ui::ButtonSize::Xs)
                            .surface(Surface::Floating)
                            .on_click(cx.listener(|this, _, window, cx| this.search(window, cx))),
                    ),
            );
        } else {
            let candidates = self.candidates(cx);
            if candidates.is_empty() {
                list = list.child(message("No people found.".into()));
            }
            for person in candidates {
                let already = members.contains(&person.pubkey);
                let archived = self.archived.contains(&person.pubkey);
                let disabled = saved || self.busy || already || archived;
                let mut detail = label(&person.pubkey);
                if already {
                    detail.push_str(" · Already has access");
                }
                if archived {
                    detail.push_str(" · Archived");
                }
                let key = person.pubkey.clone();
                let active = self.active.as_ref() == Some(&key);
                let chosen = person.clone();
                list = list.child(
                    row(
                        format!("me-share-person-{key}").into(),
                        self.name_of(&key, cx),
                        detail,
                        disabled,
                        active,
                    )
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && !already && !archived {
                            this.active = Some(key.clone());
                            cx.notify();
                        }
                    }))
                    .when(!disabled, |this| {
                        this.on_click(cx.listener(move |this, _, window, cx| {
                            this.choose(chosen.clone(), window, cx)
                        }))
                    }),
                );
            }
        }
        let anchor = self.anchor.clone();
        let dismiss = cx.listener(move |this, event: &MouseDownEvent, _, cx| {
            // A press in the search field keeps the list.
            if anchor.get().is_some_and(|b| b.contains(&event.position)) {
                return;
            }
            this.results_open = false;
            cx.notify();
        });
        let max_height = (window.viewport_size().height - bounds.bottom() - px(16.))
            .min(px(288.))
            .max(px(96.));
        Some(
            deferred(
                anchored()
                    .position(point(bounds.left(), bounds.bottom() + px(4.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("me-share-results")
                            .occlude()
                            .on_mouse_down_out(dismiss)
                            .child(
                                Popover::new()
                                    .width(PopoverWidth::Auto)
                                    .padding(PopoverPadding::List)
                                    .child(
                                        div()
                                            .id("me-share-people")
                                            .w(bounds.size.width - px(10.))
                                            .max_h(max_height)
                                            .overflow_y_scroll()
                                            .child(list),
                                    ),
                            ),
                    ),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    fn destination_select(&self, saved: bool, window: &Window, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let destinations = self.destinations(cx);
        let value = self.chosen_destination(cx).and_then(|d| match d {
            Destination::New => Some("Create new channel…".to_owned()),
            Destination::Channel(id) => destinations
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone()),
        });
        let disabled = self.busy || saved;
        let mut select = Select::new("me-share-destination")
            .placeholder("Choose a channel")
            .open(self.picking)
            .disabled(disabled)
            .subtle()
            .surface(Surface::Floating)
            .on_click(cx.listener(|this, _, _, cx| {
                this.picking = !this.picking;
                cx.notify();
            }));
        if let Some(value) = value {
            select = select.value(value);
        }
        let menu = self.picking.then(|| {
            let current = self.destination.clone();
            let mut menu = Menu::new().child(MenuLabel::new("Channels"));
            for channel in &destinations {
                let id = channel.id.clone();
                let checked = current == Some(Destination::Channel(id.clone()));
                menu = menu.child(
                    MenuItem::new(
                        SharedString::from(format!("me-share-to-{id}")),
                        channel.name.clone(),
                    )
                    .checked(checked)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.destination = Some(Destination::Channel(id.clone()));
                        this.picking = false;
                        this.error = None;
                        cx.notify();
                    })),
                );
            }
            menu = menu.child(MenuSeparator::new()).child(
                MenuItem::new("me-share-to-new", "Create new channel…")
                    .checked(current == Some(Destination::New))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.destination = Some(Destination::New);
                        this.picking = false;
                        this.error = None;
                        cx.notify();
                    })),
            );
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.picking = false;
                    cx.notify();
                });
            });
            overlay::dropdown(
                window,
                Align::Start,
                div().min_w(rems(20.)).child(menu),
                dismiss,
            )
        });
        div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .child(
                div()
                    .text_label_sm()
                    .text_color(c.text_standard)
                    .child("Share to"),
            )
            .child(div().relative().w_full().child(select).children(menu))
    }

    fn details_fields(&self, details: &DetailsDraft, locked: bool, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let errors = self.details_errors(cx).unwrap_or_default();
        let temporary = details.ttl.is_some();
        let ttl = details.ttl.unwrap_or(DEFAULT_TEMPORARY_TTL);
        let weak = cx.entity().downgrade();
        let on_duration: OnDuration = Rc::new(move |ttl, _, cx| {
            let _ = weak.update(cx, |this, cx| {
                if let Some(details) = &mut this.details {
                    details.ttl = ttl;
                }
                cx.notify();
            });
        });
        div()
            .flex()
            .flex_col()
            .gap(space::S3)
            .child(form_fields::name_field(&self.name, errors.name, locked, cx))
            .child(form_fields::duration_field(
                temporary,
                ttl,
                locked,
                errors.lifetime,
                on_duration,
                cx,
            ))
            .when(temporary, |this| {
                this.child(subtle(
                    "This conversation is automatically archived when its duration ends. Its history is not deleted.",
                    cx,
                ))
            })
            .child(
                Switch::new("me-share-private", details.private)
                    .label("Private")
                    .label_first()
                    .disabled(locked)
                    .on_change({
                        let weak = cx.entity().downgrade();
                        move |checked, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                if let Some(details) = &mut this.details {
                                    details.private = checked;
                                }
                                this.confirm_public = false;
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(subtle(
                if details.private {
                    "Only channel members can read its history and participate. Add people below (optional)."
                } else {
                    "Everyone in this community will be able to view this channel’s full history."
                },
                cx,
            ))
            .when(self.confirm_public, |this| {
                this.child(
                    div()
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child("Everyone in this community will be able to view this channel’s full history."),
                )
            })
    }
}

impl Render for SessionShare {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let session = self.session(cx);
        let saved = self.saved(cx);
        let locked = self.busy || saved.is_some();
        let direct = self.direct_share(cx);
        let eligible = self.eligible(cx);
        let new_channel = self.new_channel(cx);
        let audience = self.chosen_audience(cx);
        let chosen = self.chosen(cx);
        let details_invalid = self.details_invalid(cx);
        let creation_available = self
            .community
            .read(cx)
            .snapshot
            .relay
            .as_ref()
            .is_some_and(|relay| relay.supports_channel_creation);
        let pending_details = session
            .details_attempt(&self.channel.id)
            .is_some_and(|attempt| attempt.status == AttemptStatus::Unconfirmed);
        if !self.popup_visible(cx) || self.busy || saved.is_some() {
            self.anchor.set(None);
        }

        let mut form = div().flex().flex_col().gap(space::S3).min_w_0();
        if direct {
            form = form.child(div().child(
                "Move this conversation to Messages with its full history, Canvas and existing agents. No new conversation is created. Later agent conversations use Messages threads; the private provider session from Me does not transfer.",
            ));
            form = match self.shared_details(cx) {
                Some(details) => {
                    form.child(self.details_fields(&details, locked || self.details_loading, cx))
                }
                None => form.child(div().child(if self.details_loading {
                    "Loading channel settings…"
                } else {
                    "Channel settings could not be loaded."
                })),
            };
            if !self.details_loading && self.base.is_none() {
                form = form.child(
                    div().child(
                        Button::new("me-share-reload", "Reload channel settings")
                            .surface(Surface::Floating)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.error = None;
                                this.load_details(window, cx);
                            })),
                    ),
                );
            }
            if pending_details {
                form = form.child(
                    div().child(
                        Button::new("me-share-check", "Check channel settings status")
                            .surface(Surface::Floating)
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.check_details(window, cx)),
                            ),
                    ),
                );
            }
            if self.base.as_ref().is_some_and(|base| !base.can_edit) {
                form = form.child(div().child(
                    "Only a channel owner or admin can change these settings and share to Messages.",
                ));
            }
        } else {
            form = form.child(self.destination_select(saved.is_some(), window, cx));
        }
        if new_channel {
            let private = saved
                .as_ref()
                .and_then(|saved| saved.intent.private)
                .unwrap_or(self.new_private);
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S3)
                    .min_w_0()
                    .child(form_fields::name_field(&self.new_name, None, locked, cx))
                    .child(
                        Switch::new("me-share-new-private", private)
                            .label("Private channel")
                            .label_first()
                            .disabled(locked)
                            .on_change({
                                let weak = cx.entity().downgrade();
                                move |checked, _, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.new_private = checked;
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        div()
                            .text_label_sm()
                            .text_color(c.text_standard)
                            .child("Add to new channel"),
                    )
                    .when_some(saved.as_ref(), |this, saved| {
                        this.child(subtle(
                            format!(
                                "{} chosen for this channel",
                                saved.intent.channel_people.len()
                            ),
                            cx,
                        ))
                    })
                    .child(self.search_control(Target::Channel, locked, window, cx)),
            );
        }
        if !direct {
            let radio = |id: &'static str, label: &'static str, value: Audience| {
                Radio::new(id, label, audience == value)
                    .disabled(locked)
                    .on_select({
                        let weak = cx.entity().downgrade();
                        move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.audience = value;
                                cx.notify();
                            });
                        }
                    })
            };
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S2)
                    .child(
                        div()
                            .text_label_sm()
                            .text_color(c.text_standard)
                            .child("Who has session access"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::S3)
                            .child(radio(
                                "me-share-everyone",
                                "Everyone in this channel",
                                Audience::Everyone,
                            ))
                            .child(radio(
                                "me-share-selected",
                                "Selected people",
                                Audience::Selected,
                            )),
                    ),
            );
        }
        let audience_row = div()
            .flex()
            .items_center()
            .gap(space::S2)
            .min_h(size::CONTROL);
        form = form.child(if audience == Audience::Everyone {
            audience_row.child(selection(
                "me-share-current-members",
                "Current channel members".into(),
                cx,
            ))
        } else {
            audience_row
                .child(self.search_control(Target::Session, locked, window, cx))
                .when(!new_channel && !self.people.is_empty(), |this| {
                    this.child(
                        div()
                            .id("me-share-count")
                            .flex_none()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .cursor_pointer()
                            .child(format!("{} selected", self.people.len()))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_chosen(Target::Session, window, cx)
                            })),
                    )
                })
        });
        if new_channel && !self.channel_people.is_empty() {
            form =
                form.child(
                    selection(
                        "me-share-channel-people",
                        format!("{} for the new channel · Edit", self.channel_people.len()),
                        cx,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_chosen(Target::Channel, window, cx)
                    })),
                );
        }
        if new_channel && audience == Audience::Selected && !self.people.is_empty() {
            form =
                form.child(
                    selection(
                        "me-share-session-people",
                        format!("{} for session access · Edit", self.people.len()),
                        cx,
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_chosen(Target::Session, window, cx)
                    })),
                );
        }
        if let Some(saved) = &saved {
            if self.error.is_none() && !self.busy {
                form = form.child(subtle(retry_status(saved, &self.destinations(cx)), cx));
            }
            form = form.child(subtle(
                if direct {
                    "Starting over clears these choices, not settings or access already saved."
                } else {
                    "Starting over clears these choices, not work already submitted. Existing access, created channels, Messages placement and queued operations are kept. A new share may post another link."
                },
                cx,
            ));
        }
        if let Some(error) = &self.error {
            form = form.child(div().text_body_sm().child(error.clone()));
        } else if let Some(progress) = &self.progress {
            form = form.child(subtle(progress.clone(), cx));
        }
        if !eligible && self.error.is_none() {
            form = form.child(div().child("Session access changed. Refresh and try again."));
        }

        let disabled = self.busy
            || !eligible
            || (!direct && !new_channel && chosen.is_none())
            || details_invalid
            || (new_channel
                && saved.is_none()
                && (self.new_name(cx).is_empty() || !creation_available));
        let label = if self.confirm_public {
            "Make public and share"
        } else if self.busy {
            "Sharing…"
        } else if saved.is_some() {
            "Retry share"
        } else {
            "Share"
        };
        let popup = (self.popup_visible(cx) && !self.busy && saved.is_none())
            .then(|| self.results_popup(window, cx))
            .flatten();

        Dialog::new(
            "me-share",
            if direct {
                "Share conversation"
            } else {
                "Share session"
            },
        )
        .compact_header_gap()
        .on_close(cx.listener(|this, _, _, cx| this.close(cx)))
        .child(form)
        .children(popup)
        .footer_leading(
            Button::new("me-share-copy", self.copy_status.unwrap_or("Copy link"))
                .ghost()
                .surface(Surface::Floating)
                .disabled(self.busy)
                .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
        )
        .when(saved.is_some(), |this| {
            this.footer(
                Button::new("me-share-start-over", "Start over")
                    .surface(Surface::Floating)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.start_over(window, cx))),
            )
        })
        .footer(
            Button::new("me-share-submit", label)
                .prominent()
                .disabled(disabled)
                .on_click(cx.listener(|this, _, window, cx| this.primary(window, cx))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::EventId;
    use core::prelude::v1::test;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    fn channel(kind: ChannelType, members: &[&str]) -> ChannelSummary {
        ChannelSummary {
            id: ID.into(),
            name: "Alpha".into(),
            channel_type: Some(kind),
            private: true,
            members: members.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn labels_keys_with_the_shortest_distinct_suffix() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let labels = key_labels([a.as_str(), b.as_str(), "nope"]);
        assert_eq!(labels.len(), 2);
        let (la, lb) = (&labels[&a], &labels[&b]);
        assert!(la.starts_with("npub…") && la.chars().count() == "npub…".chars().count() + 3);
        assert_ne!(la, lb);
        assert_eq!(format_public_key(&a).as_deref(), Some(la.as_str()));
        assert_eq!(format_public_key("nope"), None);
    }

    #[test]
    fn a_direct_share_needs_a_joined_writable_conversation_and_me() {
        let viewer = "v";
        let session = channel(ChannelType::Session, &[viewer]);
        assert!(direct_eligible(Some(&session), viewer, true));
        assert!(!direct_eligible(Some(&session), viewer, false));
        assert!(!direct_eligible(Some(&session), "other", true));
        assert!(direct_eligible(
            Some(&channel(ChannelType::Stream, &[viewer])),
            viewer,
            true
        ));
        assert!(!direct_eligible(
            Some(&channel(ChannelType::Dm, &[viewer])),
            viewer,
            true
        ));
        let archived = ChannelSummary {
            archived: true,
            ..session.clone()
        };
        assert!(!direct_eligible(Some(&archived), viewer, true));
        assert!(!direct_eligible(None, viewer, true));
    }

    #[test]
    fn links_go_to_joined_streams_and_forums() {
        let viewer = "v";
        let channels = [
            channel(ChannelType::Stream, &[viewer]),
            ChannelSummary {
                id: "forum".into(),
                ..channel(ChannelType::Forum, &[viewer])
            },
            ChannelSummary {
                id: "session".into(),
                ..channel(ChannelType::Session, &[viewer])
            },
            ChannelSummary {
                id: "other".into(),
                ..channel(ChannelType::Stream, &["x"])
            },
        ];
        let ids: Vec<String> = destinations(&channels, viewer)
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, [ID, "forum"]);
    }

    #[test]
    fn a_moved_session_drops_its_marker() {
        let base = ChannelDetails {
            channel: ID.into(),
            version: EventId::from_byte_array([0; 32]),
            draft: DetailsDraft {
                name: "Alpha".into(),
                description: buzz_relay_client::session::session_description(None),
                private: true,
                ttl: None,
                labels: None,
            },
            can_edit: true,
        };
        assert_eq!(initial_draft(&base).description, "");
        let plain = ChannelDetails {
            draft: DetailsDraft {
                description: "Notes".into(),
                ..base.draft.clone()
            },
            ..base
        };
        assert_eq!(initial_draft(&plain).description, "Notes");
        assert_eq!(
            normalized(&DetailsDraft {
                name: "  #Launch ".into(),
                ..plain.draft
            })
            .name,
            "Launch"
        );
    }
}
