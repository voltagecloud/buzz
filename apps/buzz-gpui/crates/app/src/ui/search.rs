//! Search Buzz (ui.md §2.3; `PageSearch.tsx`, `SearchResults.tsx`,
//! `SearchChoices.tsx`): the header's search palette. Typed text matches
//! pages and conversations by name and searches messages on the relay;
//! `from:`, `in:`, `after:` and `before:` narrow the message search
//! (`parseSearchOperators.ts`). An author picked for `from:` shows as a
//! chip in the field. Without a ready community it still opens, offering
//! pages only.

use super::message::{channel_icon, display_name};
use super::modal::{Dismissed, Leading, OpenChannel, group_label, labeled_row, note};
use super::pages::{self, Page};
use super::search_kit::{self, Highlight, SearchUsage, match_name, match_rank, search_order};
use super::sidebar::channel_label;
use crate::community::Community;
use buzz_relay_client::members::{Person, match_tier};
use buzz_relay_client::search::{self, Day, ParsedSearch};
use buzz_relay_client::{
    ChannelSummary, ChannelType, EventId, MessageSearch, Profile, PublicChannels, RelayOrigin,
    RosterStatus, SearchOutcome, SessionStatus,
};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, ButtonSize, Icon, IconButton, IconButtonSize,
    IconName, Surface, TextField, TypeStyled,
    tokens::{radius, shadow},
};
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nostr::nips::nip19::ToBech32;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

/// Typeahead waits for a brief pause before reading the relay.
const DEBOUNCE: Duration = Duration::from_millis(180);
/// Conversations shown per name group.
const NAME_MATCHES: usize = 8;
/// People and agents the `from:` picker shows.
const AUTHOR_MATCHES: usize = 12;
/// The field's `maxLength`, in UTF-16 units as the web counts them.
const MAX_QUERY: usize = 256;
const PLACEHOLDER: &str = "Search pages, conversations and messages…";
const SCOPED_PLACEHOLDER: &str = "Search messages…";

/// Shows a page of the app.
pub type OpenPage = Rc<dyn Fn(Page, &mut Window, &mut App)>;

/// A page search offers.
struct PageEntry {
    page: Page,
    label: &'static str,
    icon: IconName,
    positions: Vec<usize>,
}

/// Every registered page in host order, then Settings, ranked and
/// underlined against the whole typed text (`PageSearch.tsx`). Every
/// palette state reads pages from here.
fn page_list(query: &str, cx: &App) -> Vec<PageEntry> {
    let needle = query.trim().to_lowercase();
    let mut ranked: Vec<(u8, PageEntry)> = pages::all(cx)
        .into_iter()
        .map(|spec| (spec.page, spec.label, spec.icon))
        .chain([(Page::SETTINGS, "Settings", IconName::Gear)])
        .filter_map(|(page, label, icon)| {
            let (rank, positions) = if needle.is_empty() {
                (0, Vec::new())
            } else {
                let found = match_name(label, &needle)?;
                (found.rank, found.positions)
            };
            Some((
                rank,
                PageEntry {
                    page,
                    label,
                    icon,
                    positions,
                },
            ))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, entry)| entry).collect()
}

/// What a result row does.
#[derive(Clone)]
enum Action {
    Open(ChannelSummary, Option<EventId>),
    /// Search only this conversation.
    Scope(ChannelSummary),
    /// Replace the query, e.g. to complete an operator.
    Query(String),
    /// Search messages by this author; `display` is the field's text once
    /// the chip replaces the typed `from:` token.
    Author {
        pubkey: String,
        name: SharedString,
        display: String,
    },
    Page(Page),
}

/// The author picked from the `from:` picker.
#[derive(Clone)]
struct Author {
    pubkey: String,
    name: SharedString,
}

/// One of the viewer's agents in this community (`agentChoices`'s
/// `selectable`), from the agent hosts.
#[derive(Clone)]
struct AgentChoice {
    pubkey: String,
    name: String,
    picture: Option<String>,
}

#[derive(Clone)]
struct Entry {
    key: String,
    leading: EntryLeading,
    label: SharedString,
    /// Code points of `label` that match the typed text.
    positions: Vec<usize>,
    detail: Option<SharedString>,
    action: Action,
    /// Choosing it is remembered for the typed text (`recordChoice`).
    remember: bool,
}

#[derive(Clone)]
enum EntryLeading {
    Icon(IconName),
    Person {
        pubkey: String,
        name: SharedString,
        picture: Option<String>,
        agent: bool,
    },
}

#[derive(Clone)]
struct Group {
    label: &'static str,
    entries: Vec<Entry>,
    empty: Option<SharedString>,
}

/// What a status note's button retries.
#[derive(Clone, Copy)]
enum Retry {
    Connection,
    Conversations,
    Channels,
    OperatorChannels,
    Messages,
}

#[derive(Clone)]
struct Status {
    text: SharedString,
    retry: Option<(&'static str, Retry)>,
}

/// One relay lookup: the answer for the request it was made for, and the
/// request in flight. A newer request drops the older one.
struct Lookup<T> {
    key: Option<String>,
    value: Option<T>,
    pending: Option<String>,
    task: Option<Task<()>>,
    /// Bumped by a retry, so the same request runs again.
    attempt: u32,
}

impl<T> Default for Lookup<T> {
    fn default() -> Self {
        Self {
            key: None,
            value: None,
            pending: None,
            task: None,
            attempt: 0,
        }
    }
}

impl<T> Lookup<T> {
    /// The request key for `base` in this attempt.
    fn request(&self, base: &str) -> String {
        format!("{}\u{0}{base}", self.attempt)
    }

    /// The answer to `base`, if it is the latest one.
    fn current(&self, base: &str) -> Option<&T> {
        let request = self.request(base);
        (self.key.as_deref() == Some(request.as_str()))
            .then_some(self.value.as_ref())
            .flatten()
    }

    /// The last answer, for whatever request.
    fn last(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// Whether `base` needs a new read; stops reads no longer wanted.
    fn wants(&mut self, base: Option<&str>) -> Option<String> {
        let Some(base) = base else {
            self.pending = None;
            self.task = None;
            return None;
        };
        let request = self.request(base);
        if self.key.as_deref() == Some(request.as_str()) {
            self.pending = None;
            self.task = None;
            return None;
        }
        (self.pending.as_deref() != Some(request.as_str())).then_some(request)
    }
}

/// How far the community is from searchable.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Connection {
    NoCommunity,
    Connecting,
    Failed,
    Ready,
}

/// What the typed query asks of the relay, worked out once per render.
struct Plan {
    query: String,
    parsed: ParsedSearch,
    /// The `from:` value still being typed.
    author_prompt: Option<String>,
    /// An `in:` name that is neither a channel id nor a joined channel, so
    /// an exact public lookup resolves it.
    operator_lookup: Option<String>,
    /// The scoped or `in:` channel.
    channel: Option<String>,
    message_search: Option<(String, MessageSearch)>,
    public_text: Option<String>,
    people: Option<(String, String)>,
    roster_ready: bool,
}

pub struct SearchDialog {
    community: Option<Entity<Community>>,
    /// The channel open behind the palette, for "Search in …".
    current: Option<ChannelSummary>,
    scope: Option<ChannelSummary>,
    input: Entity<InputState>,
    highlight: Highlight,
    /// Read once per opening, so usage recorded while it is open cannot
    /// reorder it.
    usage: SearchUsage,
    /// Shown as a chip; the query carries it as `from:<hex>`.
    author: Option<Author>,
    messages: Lookup<Result<SearchOutcome, String>>,
    public: Lookup<Result<PublicChannels, String>>,
    operator: Lookup<Result<PublicChannels, String>>,
    people: Lookup<Result<Vec<Person>, String>>,
    /// The last [`Self::groups`], until the next [`Self::refresh`]: a
    /// hover redraw does not plan the query again.
    groups: RefCell<Option<(Vec<Group>, Vec<Status>)>>,
    on_open: OpenChannel,
    on_page: OpenPage,
    /// Where focus was before the palette opened.
    return_focus: Option<FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Dismissed> for SearchDialog {}

impl SearchDialog {
    /// `scope` opens "Search this conversation" on that channel.
    pub fn new(
        community: Option<Entity<Community>>,
        current: Option<ChannelSummary>,
        scope: Option<ChannelSummary>,
        on_open: OpenChannel,
        on_page: OpenPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(community) = &community {
            super::archives::ensure(community, false, cx);
        }
        let placeholder = if scope.is_some() {
            SCOPED_PLACEHOLDER
        } else {
            PLACEHOLDER
        };
        let return_focus = window.focused(cx);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .validate(|text, _| text.encode_utf16().count() <= MAX_QUERY)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let mut subscriptions = vec![cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.changed(cx);
            }
        })];
        if let Some(community) = &community {
            subscriptions.push(cx.observe(community, |this, _, cx| {
                this.refresh(cx);
                cx.notify();
            }));
        }
        let usage = community
            .as_ref()
            .map(|community| search_kit::usage::usage_of(community.read(cx), cx))
            .unwrap_or_default();
        let mut dialog = Self {
            community,
            current,
            scope,
            input,
            highlight: Highlight::new(),
            usage,
            author: None,
            messages: Lookup::default(),
            public: Lookup::default(),
            operator: Lookup::default(),
            people: Lookup::default(),
            groups: RefCell::default(),
            on_open,
            on_page,
            return_focus,
            _subscriptions: subscriptions,
        };
        dialog.refresh(cx);
        dialog
    }

    /// The query as searched: the chip's `from:<hex>`, then the typed text
    /// (`SearchChoices.tsx` maps edits back the same way).
    fn query(&self, cx: &App) -> String {
        let text = self.display(cx);
        match &self.author {
            Some(author) => format!("from:{} {}", author.pubkey, text.trim_start()),
            None => text,
        }
    }

    /// What the field shows.
    fn display(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        cx.notify();
    }

    /// Closes the palette. Focus returns to where it was, unless the
    /// destination just chose took it.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input_focused = self.input.focus_handle(cx).is_focused(window);
        if (input_focused || window.focused(cx).is_none())
            && let Some(handle) = &self.return_focus
        {
            window.focus(handle, cx);
        }
        cx.emit(Dismissed);
    }

    fn connection(&self, cx: &App) -> Connection {
        let Some(community) = &self.community else {
            return Connection::NoCommunity;
        };
        match community.read(cx).snapshot.status {
            SessionStatus::Ready => Connection::Ready,
            SessionStatus::Connecting => Connection::Connecting,
            SessionStatus::Failed(_) => Connection::Failed,
        }
    }

    /// The viewer's agents in this community, from every agent host.
    fn agents(&self, cx: &App) -> Vec<AgentChoice> {
        let Some(community) = &self.community else {
            return Vec::new();
        };
        let origin = &community.read(cx).origin;
        crate::ui::identity::AgentLibrary::from_hosts(cx)
            .agents
            .into_iter()
            .filter(|(_, _, relay)| RelayOrigin::parse(relay).is_ok_and(|o| o == *origin))
            .map(|(pubkey, name, _)| AgentChoice {
                pubkey,
                name,
                picture: None,
            })
            .collect()
    }

    /// Works out the relay reads the query needs (`SearchResults.tsx`,
    /// `useSearchMessages.ts`, `usePublicChannelSearch.ts`). `None` until
    /// the community is ready.
    fn plan(&self, cx: &App) -> Option<Plan> {
        if self.connection(cx) != Connection::Ready {
            return None;
        }
        let community = self.community.as_ref()?.read(cx);
        let snapshot = &community.snapshot;
        let query = self.query(cx);
        let parsed = search::parse(query.trim());
        let author_prompt = search::unfinished_author(&query);
        let roster_ready = snapshot.roster == RosterStatus::Ready;
        let scope = self.scope.as_ref().map(|s| s.id.clone());

        // `in:` resolves to a channel id, a joined name, or an exact public
        // lookup.
        let named = parsed
            .channel_name()
            .filter(|_| scope.is_none())
            .map(str::to_owned);
        let mut operator_lookup = None;
        let mut operator_channel = None;
        if let Some(name) = &named {
            if search::is_channel_uuid(name) {
                operator_channel = Some(name.to_ascii_lowercase());
            } else {
                let wanted = name.to_lowercase();
                let local = snapshot.channels.iter().find(|c| {
                    c.name.to_lowercase() == wanted
                        || channel_label(c, &snapshot.profiles).to_lowercase() == wanted
                });
                match local {
                    Some(channel) => operator_channel = Some(channel.id.clone()),
                    None => {
                        operator_lookup = Some(name.clone());
                        if let Some(Ok(found)) = self.operator.current(name) {
                            operator_channel = found.channels.first().map(|c| c.id.clone());
                        }
                    }
                }
            }
        }
        let channel = scope.clone().or(operator_channel);
        let unresolved = named.is_some() && channel.is_none();
        let can_search =
            author_prompt.is_none() && (!parsed.text.is_empty() || parsed.has_filters());
        let message_search = (can_search && !unresolved).then(|| {
            let from = parsed.from.as_deref().map(|value| {
                if search::is_hex_key(value) {
                    value.to_ascii_lowercase()
                } else {
                    value.to_string()
                }
            });
            let (since, until) = parsed.range(local_midnight);
            let request = MessageSearch {
                text: parsed.text.clone(),
                from,
                channel: channel.clone(),
                since,
                until,
                agents: self
                    .agents(cx)
                    .into_iter()
                    .map(|agent| (agent.pubkey, agent.name))
                    .collect(),
            };
            (format!("{request:?}"), request)
        });
        let public_text = (scope.is_none() && !parsed.text.is_empty() && roster_ready)
            .then(|| parsed.text.clone());

        // The `from:` picker: a name being typed, or a completed one that
        // matched several people.
        let ambiguous = author_prompt.is_none()
            && message_search.as_ref().is_some_and(
                |(key, _)| matches!(self.messages.current(key), Some(Ok(o)) if o.ambiguous_author),
            );
        let needle = match &author_prompt {
            Some(value) => Some(value.clone()),
            None if ambiguous => parsed.from.clone(),
            None => None,
        };
        let people = needle.map(|needle| {
            let needle = needle.trim_start_matches('@').to_lowercase();
            let key = format!("{needle}\u{0}{}", channel.clone().unwrap_or_default());
            (key, needle)
        });
        Some(Plan {
            query,
            parsed,
            author_prompt,
            operator_lookup,
            channel,
            message_search,
            public_text,
            people,
            roster_ready,
        })
    }

    /// Starts the reads the query needs and drops those it no longer does.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Every change to what the groups read comes through here.
        self.groups.get_mut().take();
        let plan = self.plan(cx);
        let Some(community) = self.community.clone().filter(|_| plan.is_some()) else {
            self.messages.wants(None);
            self.public.wants(None);
            self.operator.wants(None);
            self.people.wants(None);
            return;
        };
        let Some(plan) = plan else { return };
        let session = community.read(cx).session.clone();

        let operator = plan
            .operator_lookup
            .as_deref()
            .filter(|_| plan.roster_ready);
        if let Some(request) = self.operator.wants(operator) {
            let (session, name) = (session.clone(), operator.unwrap_or_default().to_owned());
            self.spawn(|this| &mut this.operator, request, cx, async move {
                session.search_public_channel_page(&name, true).await
            });
        }

        let message_key = plan.message_search.as_ref().map(|(key, _)| key.as_str());
        if let Some(request) = self.messages.wants(message_key) {
            let (session, search) = (session.clone(), plan.message_search.clone().unwrap().1);
            self.spawn(|this| &mut this.messages, request, cx, async move {
                session.search_messages(search).await
            });
        }

        if let Some(request) = self.public.wants(plan.public_text.as_deref()) {
            let (session, text) = (
                session.clone(),
                plan.public_text.clone().unwrap_or_default(),
            );
            self.spawn(|this| &mut this.public, request, cx, async move {
                session.search_public_channel_page(&text, false).await
            });
        }

        let people_key = plan.people.as_ref().map(|(key, _)| key.as_str());
        if let Some(request) = self.people.wants(people_key) {
            let needle = plan
                .people
                .clone()
                .map(|(_, needle)| needle)
                .unwrap_or_default();
            self.spawn(|this| &mut this.people, request, cx, async move {
                if needle.is_empty() {
                    Ok(Vec::new())
                } else {
                    session
                        .search_members(&needle, 1)
                        .await
                        .map(|page| page.people)
                }
            });
        }
    }

    /// Runs `read` after the typing pause and keeps its answer if it is
    /// still the request in flight.
    fn spawn<T: 'static>(
        &mut self,
        slot: fn(&mut Self) -> &mut Lookup<T>,
        request: String,
        cx: &mut Context<Self>,
        read: impl Future<Output = T> + 'static,
    ) {
        let key = request.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let value = read.await;
            let _ = this.update(cx, |this, cx| {
                let lookup = slot(this);
                if lookup.pending.as_deref() == Some(key.as_str()) {
                    lookup.key = Some(key);
                    lookup.value = Some(value);
                    lookup.pending = None;
                    lookup.task = None;
                    this.refresh(cx);
                    cx.notify();
                }
            });
        });
        let lookup = slot(self);
        lookup.pending = Some(request);
        lookup.task = Some(task);
    }

    fn retry(&mut self, retry: Retry, window: &mut Window, cx: &mut Context<Self>) {
        // A retry removes its own button; the field keeps the keyboard.
        self.input.update(cx, |state, cx| state.focus(window, cx));
        match retry {
            Retry::Connection | Retry::Conversations => {
                if let Some(community) = &self.community {
                    community.read(cx).session.retry();
                }
            }
            Retry::Channels => self.public.attempt += 1,
            Retry::OperatorChannels => self.operator.attempt += 1,
            Retry::Messages => self.messages.attempt += 1,
        }
        self.refresh(cx);
        cx.notify();
    }

    fn set_query(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.focus(window, cx);
        });
        self.changed(cx);
    }

    fn run(&mut self, entry: (String, Action, bool), window: &mut Window, cx: &mut Context<Self>) {
        let (key, action, remember) = entry;
        if remember && let Some(community) = &self.community {
            let needle = name_needle(&search::parse(self.query(cx).trim()));
            search_kit::record_choice(community.read(cx), &needle, &key, cx);
        }
        match action {
            Action::Open(channel, message) => {
                (self.on_open)(&channel, message, window, cx);
                self.close(window, cx);
            }
            Action::Scope(channel) => {
                self.scope = Some(channel);
                self.input.update(cx, |state, cx| {
                    state.set_placeholder(SCOPED_PLACEHOLDER, window, cx);
                });
                self.set_query(String::new(), window, cx);
            }
            Action::Query(text) => self.set_query(text, window, cx),
            Action::Author {
                pubkey,
                name,
                display,
            } => {
                self.author = Some(Author { pubkey, name });
                self.set_query(display, window, cx);
            }
            Action::Page(page) => {
                (self.on_page)(page, window, cx);
                self.close(window, cx);
            }
        }
    }

    fn unscope(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scope = None;
        self.input.update(cx, |state, cx| {
            state.set_placeholder(PLACEHOLDER, window, cx);
            state.focus(window, cx);
        });
        self.changed(cx);
    }

    /// The chip's ×: the author no longer narrows the search.
    fn remove_author(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.author = None;
        self.input.update(cx, |state, cx| state.focus(window, cx));
        self.changed(cx);
    }

    /// The `from:` picker (`SearchResults.tsx:248-445`): the effective
    /// channel's members, the viewer's agents and the relay's people whose
    /// names match, accents and case folded; exact names, then people
    /// before agents, then members. An agent also matches by its own name.
    fn author_groups(&self, plan: &Plan, cx: &App) -> Vec<Group> {
        let Some(community) = &self.community else {
            return Vec::new();
        };
        let snapshot = &community.read(cx).snapshot;
        let Some((people_key, needle)) = &plan.people else {
            return Vec::new();
        };
        let display = self.display(cx);
        let display_after = author_display(&display, plan.author_prompt.is_some());
        let members: Vec<String> = plan
            .channel
            .as_ref()
            .and_then(|id| snapshot.channels.iter().find(|c| c.id == *id))
            .map(|channel| channel.members.clone())
            .unwrap_or_default();
        let agents = self.agents(cx);
        let agent_named = |pubkey: &str| agents.iter().find(|a| a.pubkey == pubkey);
        let mut candidates: Vec<(String, Profile)> = members
            .iter()
            .filter_map(|key| Some((key.clone(), (**snapshot.profiles.get(key)?).clone())))
            .collect();
        let mut add = |key: &str, profile: Profile| {
            if !candidates.iter().any(|(k, _)| k == key) {
                candidates.push((key.to_owned(), profile));
            }
        };
        for agent in &agents {
            let profile = snapshot
                .profiles
                .get(&agent.pubkey)
                .map(|p| (**p).clone())
                .unwrap_or_else(|| Profile {
                    name: agent.name.clone(),
                    is_agent: true,
                    picture: agent.picture.clone(),
                    ..Default::default()
                });
            add(&agent.pubkey, profile);
        }
        let current = self.people.current(people_key);
        if let Some(Ok(people)) = current {
            for person in people {
                add(&person.pubkey, person.profile.clone());
            }
        }
        let tier = |key: &str, profile: &Profile| {
            [
                Some(profile.name.as_str()),
                agent_named(key).map(|a| a.name.as_str()),
            ]
            .into_iter()
            .flatten()
            .filter_map(|alias| match_tier(alias, needle))
            .min()
            .unwrap_or(u8::MAX)
        };
        let is_agent =
            |key: &str, profile: &Profile| profile.is_agent || agent_named(key).is_some();
        let mut ranked: Vec<(u8, String, Profile)> = if current.is_some() {
            candidates
                .into_iter()
                .filter(|(key, _)| !super::archives::hides(community, key, cx))
                .map(|(key, profile)| (tier(&key, &profile), key, profile))
                .filter(|(tier, ..)| *tier < 4)
                .collect()
        } else {
            Vec::new()
        };
        ranked.sort_by_key(|(tier, key, profile)| {
            (*tier != 0, is_agent(key, profile), !members.contains(key))
        });
        ranked.truncate(AUTHOR_MATCHES);
        let media_less = |key: &str, profile: &Profile| {
            profile
                .picture
                .clone()
                .or_else(|| agent_named(key).and_then(|a| a.picture.clone()))
        };
        let entry = |key: &String, profile: &Profile| Entry {
            key: format!("author:{key}"),
            leading: EntryLeading::Person {
                pubkey: key.clone(),
                name: profile.name.clone().into(),
                picture: media_less(key, profile),
                agent: is_agent(key, profile),
            },
            label: profile.name.clone().into(),
            positions: Vec::new(),
            detail: Some(key.chars().take(12).collect::<String>().into()),
            action: Action::Author {
                pubkey: key.clone(),
                name: profile.name.clone().into(),
                display: display_after.clone(),
            },
            remember: false,
        };
        let (agent_rows, people_rows): (Vec<_>, Vec<_>) = ranked
            .iter()
            .partition(|(_, key, profile)| is_agent(key, profile));
        let people: Vec<Entry> = people_rows.iter().map(|(_, k, p)| entry(k, p)).collect();
        let agents: Vec<Entry> = agent_rows.iter().map(|(_, k, p)| entry(k, p)).collect();
        let empty = if !people.is_empty() || !agents.is_empty() {
            None
        } else {
            match current {
                None => Some("Searching people…"),
                Some(Err(_)) => Some("People search is unavailable. Try a longer name."),
                Some(Ok(_)) if !needle.is_empty() => {
                    Some("No matching people. Try a different name.")
                }
                Some(Ok(_)) => Some("Type a name to search people."),
            }
        };
        vec![
            Group {
                label: "People",
                entries: people,
                empty: empty.map(Into::into),
            },
            Group {
                label: "Agents",
                entries: agents,
                empty: None,
            },
        ]
    }

    /// The palette without a ready community (`PageSearch.tsx`
    /// `CommunitySearch`): pages only, with the reason.
    fn offline_groups(&self, cx: &App) -> (Vec<Group>, Vec<Status>) {
        let connection = self.connection(cx);
        let query = self.query(cx);
        let page = |entry: PageEntry| page_entry(entry, false);
        let groups = if self.scope.is_some() {
            vec![Group {
                label: "Most relevant",
                entries: Vec::new(),
                empty: Some("Connect to this community to search messages.".into()),
            }]
        } else if !query.trim().is_empty() {
            vec![Group {
                label: "Pages",
                entries: page_list(&query, cx).into_iter().map(page).collect(),
                empty: Some("No matching pages.".into()),
            }]
        } else {
            vec![
                Group {
                    label: "Recent activity",
                    entries: Vec::new(),
                    empty: Some(if connection == Connection::NoCommunity {
                        "Choose a community to see recent conversations.".into()
                    } else {
                        "Connecting to this community…".into()
                    }),
                },
                Group {
                    label: "Actions",
                    entries: page_list(&query, cx).into_iter().map(page).collect(),
                    empty: None,
                },
            ]
        };
        let status = (connection == Connection::Failed)
            .then(|| Status {
                text: "Message search is unavailable while this community is disconnected.".into(),
                retry: Some(("Retry connection", Retry::Connection)),
            })
            .into_iter()
            .collect();
        (groups, status)
    }

    fn groups(&self, cx: &App) -> (Vec<Group>, Vec<Status>) {
        if let Some(groups) = &*self.groups.borrow() {
            return groups.clone();
        }
        let groups = self.build_groups(cx);
        *self.groups.borrow_mut() = Some(groups.clone());
        groups
    }

    fn build_groups(&self, cx: &App) -> (Vec<Group>, Vec<Status>) {
        let (Some(plan), Some(community)) = (self.plan(cx), &self.community) else {
            return self.offline_groups(cx);
        };
        let community = community.read(cx);
        let snapshot = &community.snapshot;
        let profiles = &snapshot.profiles;
        let query = &plan.query;
        let parsed = &plan.parsed;
        let scoped = self.scope.is_some();
        let display = self.display(cx);
        let roster = &snapshot.roster;

        // Live channels, and archived ones the viewer can still write in.
        let channels: Vec<&ChannelSummary> = snapshot
            .channels
            .iter()
            .filter(|c| {
                (!c.archived
                    || (!c.read_only
                        && matches!(
                            c.channel_type,
                            Some(ChannelType::Stream | ChannelType::Forum | ChannelType::Session)
                        )))
                    && (!c.hidden || c.channel_type == Some(ChannelType::Dm))
                    && self.scope.as_ref().is_none_or(|s| s.id == c.id)
            })
            .collect();
        let label = |channel: &ChannelSummary| channel_label(channel, profiles);
        let needle = name_needle(parsed);

        let mut status = Vec::new();
        if *roster == RosterStatus::Loading
            && !query.trim().is_empty()
            && snapshot.channels.is_empty()
        {
            status.push(Status {
                text: "Loading joined conversations…".into(),
                retry: None,
            });
        }
        if matches!(roster, RosterStatus::Error(_)) {
            status.push(Status {
                text: "Couldn’t load all joined conversations.".into(),
                retry: Some(("Retry conversations", Retry::Conversations)),
            });
        }
        // The roster is complete only once its scan finished.
        if *roster != RosterStatus::Ready {
            status.push(Status {
                text: "Conversation names include only loaded joined conversations.".into(),
                retry: None,
            });
        }

        // Public channels: this lookup's answer, or while the next one runs,
        // the last answer's channels that still match. A channel joined
        // since leaves them.
        let joined: HashSet<&str> = snapshot.channels.iter().map(|c| c.id.as_str()).collect();
        let public_current = plan
            .public_text
            .as_deref()
            .and_then(|text| self.public.current(text));
        let public_loading = plan.public_text.is_some() && public_current.is_none();
        let public: Vec<&ChannelSummary> = match public_current {
            Some(Ok(found)) => found.channels.iter().collect(),
            Some(Err(_)) => Vec::new(),
            None if public_loading => {
                let mut kept: Vec<&ChannelSummary> = match self.public.last() {
                    Some(Ok(found)) => found
                        .channels
                        .iter()
                        .filter(|c| !needle.is_empty() && c.name.to_lowercase().contains(&needle))
                        .collect(),
                    _ => Vec::new(),
                };
                kept.sort_by_key(|c| match_rank(&c.name, &needle).unwrap_or(3));
                kept
            }
            None => Vec::new(),
        }
        .into_iter()
        .filter(|c| !joined.contains(c.id.as_str()))
        .collect();
        let operator_current = plan
            .operator_lookup
            .as_deref()
            .and_then(|name| self.operator.current(name));
        let operator_pending =
            plan.operator_lookup.is_some() && (!plan.roster_ready || operator_current.is_none());
        let operator_error = match operator_current {
            Some(Err(error)) => Some(public_error(error)),
            _ => None,
        };
        let public_partial = matches!(public_current, Some(Ok(found)) if found.partial)
            || matches!(operator_current, Some(Ok(found)) if found.partial);
        if !query.trim().is_empty() && !scoped && public_partial {
            status.push(Status {
                text: "Public channel results include only the first page of channels.".into(),
                retry: None,
            });
        }
        if !scoped && let Some(Err(error)) = public_current {
            status.push(Status {
                text: public_error(error).into(),
                retry: Some(("Retry channels", Retry::Channels)),
            });
        }
        if let Some(error) = &operator_error {
            status.push(Status {
                text: error.clone().into(),
                retry: Some(("Retry channels", Retry::OperatorChannels)),
            });
        }

        // Message hits, in conversations the viewer can still read.
        let message_current = plan
            .message_search
            .as_ref()
            .and_then(|(key, _)| self.messages.current(key));
        let messages_loading = plan.message_search.is_some() && message_current.is_none();
        let message_error = match message_current {
            Some(Err(error)) => Some(message_error(error, scoped)),
            _ => None,
        };
        if let Some(error) = &message_error {
            status.push(Status {
                text: error.clone().into(),
                retry: Some(("Retry messages", Retry::Messages)),
            });
        }
        let messages: Vec<Entry> = match message_current {
            Some(Ok(outcome)) => outcome
                .hits
                .iter()
                .filter_map(|hit| {
                    let channel = snapshot
                        .channels
                        .iter()
                        .find(|c| c.id == hit.channel)
                        .or_else(|| {
                            outcome
                                .channels
                                .iter()
                                .find(|c| c.id == hit.channel && c.read_only)
                        })?;
                    let date = Local
                        .timestamp_opt(hit.created_at as i64, 0)
                        .single()
                        .map(|t| t.format("%-m/%-d/%Y").to_string())
                        .unwrap_or_default();
                    let sender = match &hit.workflow_owner {
                        Some(owner) => {
                            format!("Workflow · owned by {}", display_name(profiles, owner))
                        }
                        None => display_name(profiles, &hit.author).to_string(),
                    };
                    Some(Entry {
                        key: hit.id.to_hex(),
                        leading: EntryLeading::Icon(IconName::ChatCircle),
                        label: hit.preview.clone().into(),
                        positions: Vec::new(),
                        detail: Some(format!("{} · {sender} · {date}", label(channel)).into()),
                        action: Action::Open(channel.clone(), Some(hit.id)),
                        remember: false,
                    })
                })
                .collect(),
            _ => Vec::new(),
        };
        let message_empty: SharedString = if operator_pending || messages_loading {
            "Searching messages…".into()
        } else if operator_error.is_some() || message_error.is_some() {
            "Message search is unavailable.".into()
        } else if !query.trim().is_empty() {
            "No matching messages in accessible conversations.".into()
        } else if scoped {
            "Type to search messages in this conversation.".into()
        } else {
            "Type to search messages in this community.".into()
        };

        // A `from:` being typed, or one that matched several people, picks
        // a person.
        if plan.people.is_some() {
            return (self.author_groups(&plan, cx), status);
        }

        // An `after:` or `before:` still being typed picks a day.
        if let Some((prefix, operator)) = date_prompt(&display) {
            let entries = date_presets(Local::now().date_naive())
                .into_iter()
                .map(|(label, day)| {
                    let date = day.format("%Y-%m-%d").to_string();
                    Entry {
                        key: format!("date:{label}"),
                        leading: EntryLeading::Icon(IconName::Calendar),
                        label: label.into(),
                        positions: Vec::new(),
                        detail: Some(date.clone().into()),
                        action: Action::Query(format!("{prefix}{operator}:{date} ")),
                        remember: false,
                    }
                })
                .collect();
            let groups = vec![Group {
                label: "Dates",
                entries,
                empty: None,
            }];
            return (groups, status);
        }

        if scoped {
            let groups = vec![Group {
                label: "Most relevant",
                empty: Some(message_empty),
                entries: messages,
            }];
            return (groups, status);
        }

        let pages = page_list(query, cx);
        // What the viewer chose before for this typed text, among rows
        // that match, so a stale choice cannot add a row.
        let candidates: HashSet<String> = channels
            .iter()
            .filter(|c| !parsed.text.is_empty() && match_rank(&label(c), &needle).is_some())
            .map(|c| format!("channel:{}", c.id))
            .chain(public.iter().map(|c| format!("channel:{}", c.id)))
            .chain(pages.iter().map(|p| p.page.key().to_owned()))
            .collect();
        let picked = self.usage.pick(&needle, &candidates);
        // The shared search order; archived channels half a step behind
        // live ones in the same place.
        let rank_of = |label: &str, key: &str, archived: bool| {
            let rank = f64::from(match_rank(label, &needle).unwrap_or(6));
            search_order(&self.usage, picked.as_deref(), key, rank)
                + if archived { 0.5 } else { 0.0 }
        };
        let channel_rank =
            |c: &ChannelSummary| rank_of(&label(c), &format!("channel:{}", c.id), c.archived);
        let conversation = |channel: &ChannelSummary| {
            let name = label(channel);
            let positions = if needle.is_empty() {
                Vec::new()
            } else {
                match_name(&name, &needle)
                    .map(|found| found.positions)
                    .unwrap_or_default()
            };
            Entry {
                key: format!("channel:{}", channel.id),
                leading: EntryLeading::Icon(IconName::ChatCircle),
                label: name,
                positions,
                detail: Some(conversation_detail(channel).into()),
                action: Action::Open(channel.clone(), None),
                remember: true,
            }
        };

        let scope_action = self
            .current
            .as_ref()
            .filter(|current| channels.iter().any(|c| c.id == current.id))
            .map(|channel| Entry {
                key: format!("scope:{}", channel.id),
                leading: EntryLeading::Icon(IconName::ChatCircle),
                label: format!(
                    "Search {} {}",
                    if channel.channel_type == Some(ChannelType::Dm) {
                        "conversation with"
                    } else {
                        "in"
                    },
                    label(channel)
                )
                .into(),
                positions: Vec::new(),
                detail: Some("Search messages in this conversation".into()),
                action: Action::Scope(channel.clone()),
                remember: false,
            });

        if query.trim().is_empty() {
            let mut recent: Vec<&ChannelSummary> = channels
                .iter()
                .copied()
                .filter(|c| !c.read_only && !c.archived)
                .collect();
            // Newest message first (`lastActivityAt ?? updatedAt`).
            let activity = |c: &ChannelSummary| {
                snapshot
                    .unread
                    .latest
                    .get(&c.id)
                    .copied()
                    .or(c.last_activity_at)
                    .or(c.updated_at)
                    .unwrap_or(0)
            };
            recent.sort_by_key(|c| std::cmp::Reverse(activity(c)));
            let mut groups: Vec<Group> = Vec::new();
            if let Some(entry) = scope_action {
                groups.push(Group {
                    label: "This conversation",
                    entries: vec![entry],
                    empty: None,
                });
            }
            groups.push(Group {
                label: "Recent activity",
                entries: recent
                    .into_iter()
                    .take(4)
                    .map(|c| {
                        let mut entry = conversation(c);
                        // The last message, when known, says more than the type.
                        if let Some(preview) = snapshot
                            .unread
                            .previews
                            .get(&c.id)
                            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                            .filter(|text| !text.is_empty())
                        {
                            entry.detail = Some(preview.into());
                        }
                        entry
                    })
                    .collect(),
                empty: Some(if *roster == RosterStatus::Loading {
                    "Loading recent conversations…".into()
                } else {
                    "No recent activity yet.".into()
                }),
            });
            groups.push(Group {
                label: "Actions",
                entries: pages.into_iter().map(|p| page_entry(p, true)).collect(),
                empty: None,
            });
            return (groups, status);
        }

        // Rank before the limit, so an exact name beyond the first eight
        // still shows.
        let by_rank = |rows: &mut Vec<&ChannelSummary>| {
            rows.sort_by(|a, b| channel_rank(a).total_cmp(&channel_rank(b)));
        };
        let mut matching_all: Vec<&ChannelSummary> = channels
            .iter()
            .copied()
            .filter(|c| !parsed.text.is_empty() && match_rank(&label(c), &needle).is_some())
            .collect();
        by_rank(&mut matching_all);
        let matching: Vec<&ChannelSummary> =
            matching_all.iter().copied().take(NAME_MATCHES).collect();
        // Joined and public channels share one ranking before the limit;
        // joined channels win ties.
        let joined_matches: Vec<&ChannelSummary> = matching_all
            .iter()
            .copied()
            .filter(|c| c.channel_type != Some(ChannelType::Dm))
            .collect();
        let mut channel_results: Vec<&ChannelSummary> = joined_matches
            .iter()
            .copied()
            .chain(
                public
                    .iter()
                    .copied()
                    .filter(|c| !joined_matches.iter().any(|j| j.id == c.id)),
            )
            .collect();
        by_rank(&mut channel_results);
        channel_results.truncate(NAME_MATCHES);
        let dms: Vec<&ChannelSummary> = matching
            .iter()
            .copied()
            .filter(|c| c.channel_type == Some(ChannelType::Dm))
            .collect();
        let mut ranked_pages: Vec<(f64, PageEntry)> = pages
            .into_iter()
            .map(|p| (rank_of(p.label, p.page.key(), false), p))
            .collect();
        ranked_pages.sort_by(|a, b| a.0.total_cmp(&b.0));
        let best = |ranks: Vec<f64>| ranks.into_iter().fold(f64::INFINITY, f64::min);
        let any_named =
            !matching.is_empty() || !channel_results.is_empty() || !ranked_pages.is_empty();
        let mut named = [
            (
                best(channel_results.iter().map(|c| channel_rank(c)).collect()),
                Group {
                    label: "Channels",
                    entries: channel_results.iter().map(|c| conversation(c)).collect(),
                    empty: None,
                },
            ),
            (
                best(dms.iter().map(|c| channel_rank(c)).collect()),
                Group {
                    label: "Direct messages",
                    entries: dms.iter().map(|c| conversation(c)).collect(),
                    empty: None,
                },
            ),
            (
                best(ranked_pages.iter().map(|(rank, _)| *rank).collect()),
                Group {
                    label: "Pages",
                    entries: ranked_pages
                        .into_iter()
                        .map(|(_, p)| page_entry(p, true))
                        .collect(),
                    empty: None,
                },
            ),
        ];
        // The group with the best match leads, so Enter opens it.
        named.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut groups: Vec<Group> = named.into_iter().map(|(_, g)| g).collect();
        if let Some(entry) = scope_action {
            groups.push(Group {
                label: "This conversation",
                entries: vec![entry],
                empty: None,
            });
        }
        groups.push(Group {
            label: "Most relevant",
            empty: (!any_named).then_some(message_empty),
            entries: messages,
        });
        (groups, status)
    }
}

/// Typed text as names match it: the free text, lowercased, without a
/// leading `#`.
fn name_needle(parsed: &ParsedSearch) -> String {
    let lower = parsed.text.to_lowercase();
    lower.strip_prefix('#').unwrap_or(&lower).to_owned()
}

fn page_entry(entry: PageEntry, remember: bool) -> Entry {
    Entry {
        key: entry.page.key().to_owned(),
        leading: EntryLeading::Icon(entry.icon),
        label: entry.label.into(),
        positions: entry.positions,
        detail: None,
        action: Action::Page(entry.page),
        remember,
    }
}

/// `usePublicChannelSearch`'s error text.
fn public_error(error: &str) -> String {
    if error.is_empty() {
        "Public channel search couldn’t finish.".into()
    } else {
        format!(
            "Public channel search couldn’t finish: {}",
            error.chars().take(240).collect::<String>()
        )
    }
}

/// `useSearchMessages`'s error text.
fn message_error(error: &str, scoped: bool) -> String {
    let reason = if error.is_empty() {
        ".".to_owned()
    } else {
        format!(": {}", error.chars().take(240).collect::<String>())
    };
    let after = if scoped {
        " Try again."
    } else {
        " Pages and conversations are still available."
    };
    format!("Message search couldn’t finish{reason}{after}")
}

/// The field's text once a picked author's chip replaces the `from:`
/// token: the unfinished token at the end, or else the last completed one
/// (whose name matched several people). One space after it goes too.
fn author_display(display: &str, unfinished: bool) -> String {
    let mut tokens = Vec::new();
    let mut start = None;
    for (at, c) in display.char_indices().chain([(display.len(), ' ')]) {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(at),
            (true, Some(from)) => {
                tokens.push(from..at);
                start = None;
            }
            _ => {}
        }
    }
    let is_author = |range: &std::ops::Range<usize>| {
        let token = &display[range.clone()];
        token
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("from:"))
    };
    let found = if unfinished {
        tokens
            .last()
            .filter(|range| range.end == display.len() && is_author(range))
    } else {
        tokens.iter().rev().find(|range| {
            is_author(range)
                && !display[(*range).clone()][5..]
                    .trim_start_matches('@')
                    .is_empty()
        })
    };
    match found {
        Some(range) => {
            let after = &display[range.end..];
            format!(
                "{}{}",
                &display[..range.start],
                after.strip_prefix(' ').unwrap_or(after)
            )
        }
        // `from:` alone at the end of an otherwise empty field.
        None if unfinished => display
            .rfind("from:")
            .map(|at| display[..at].to_owned())
            .unwrap_or_default(),
        None => display.to_owned(),
    }
}

/// The Unix time of a day's local midnight.
fn local_midnight(day: Day) -> Option<u64> {
    let date = NaiveDate::from_ymd_opt(day.year, day.month, day.day)?;
    let midnight = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()?;
    u64::try_from(midnight.timestamp()).ok()
}

/// The text before an unfinished `after:`/`before:` token, and which one.
fn date_prompt(query: &str) -> Option<(String, &'static str)> {
    let token_start = query.rfind(char::is_whitespace).map_or(0, |at| at + 1);
    let token = &query[token_start..];
    let (operator, value) = token.split_once(':')?;
    let operator = match operator.to_ascii_lowercase().as_str() {
        "after" => "after",
        "before" => "before",
        _ => return None,
    };
    Day::parse(value)
        .is_none()
        .then(|| (query[..token_start].to_string(), operator))
}

/// Today, yesterday, this week, last week and this month (weeks start on
/// Monday).
fn date_presets(today: NaiveDate) -> Vec<(&'static str, NaiveDate)> {
    let monday = today - chrono::Days::new(u64::from(today.weekday().num_days_from_monday()));
    vec![
        ("Today", today),
        ("Yesterday", today.pred_opt().unwrap_or(today)),
        ("This week", monday),
        ("Last week", monday - chrono::Days::new(7)),
        ("This month", today.with_day(1).unwrap_or(today)),
    ]
}

fn conversation_detail(channel: &ChannelSummary) -> &'static str {
    if channel.archived {
        "Archived channel"
    } else if channel.read_only && !channel.cached {
        "Public channel · not joined"
    } else {
        match channel.channel_type {
            Some(ChannelType::Dm) => "Direct message",
            Some(ChannelType::Session) => "Session",
            _ => "Conversation",
        }
    }
}

/// Shift+Mod+1…9 by the digit key. Some platforms report the shifted
/// symbol (US layout) and consume Shift into it.
fn shortcut_number(key: &str, shift: bool) -> Option<usize> {
    const SHIFTED: [&str; 9] = ["!", "@", "#", "$", "%", "^", "&", "*", "("];
    let digit = match key.parse::<usize>() {
        Ok(digit) if shift => Some(digit),
        Ok(_) => None,
        Err(_) => SHIFTED.iter().position(|s| *s == key).map(|at| at + 1),
    }?;
    (1..=9).contains(&digit).then_some(digit)
}

impl SearchDialog {
    /// The search field (`SearchChoices.tsx`): magnifier, the author chip
    /// when one is picked, the text, and a clear button.
    fn field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let chip = self.author.as_ref().map(|author| {
            let npub = nostr::prelude::PublicKey::from_hex(&author.pubkey)
                .ok()
                .and_then(|key| key.to_bech32().ok())
                .unwrap_or_else(|| author.pubkey.clone());
            div()
                .id("search-author-chip")
                .max_w(px(240.))
                .px(rems(0.5))
                .py(rems(0.125))
                .rounded(radius::CHIP)
                .bg(c.affordance_subtle)
                .hover(|s| s.bg(c.affordance_subtle_hover))
                .text_body_sm()
                .text_color(c.text_link)
                .truncate()
                .cursor_pointer()
                .tooltip(buzz_ui::Tooltip::text(npub))
                .child(format!("× from:@{}", author.name))
                .on_click(cx.listener(|this, _, window, cx| this.remove_author(window, cx)))
        });
        let input = self.input.clone();
        let clear = (!self.display(cx).is_empty()).then(|| {
            IconButton::new("search-clear", IconName::X)
                .size(IconButtonSize::Sm)
                .surface(Surface::Floating)
                .on_click(move |_, window, cx| {
                    input.update(cx, |state, cx| {
                        state.set_value("", window, cx);
                        state.focus(window, cx);
                    });
                })
        });
        TextField::new(&self.input)
            .surface(Surface::Floating)
            .leading(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .child(Icon::new(IconName::MagnifyingGlass).size(16.))
                    .children(chip),
            )
            .when_some(clear, |this, clear| this.trailing(clear))
    }

    fn key_down(
        &mut self,
        event: &KeyDownEvent,
        keys: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        if m.secondary()
            && !m.alt
            && (if cfg!(target_os = "macos") {
                !m.control
            } else {
                !m.platform
            })
            && let Some(number) = shortcut_number(&keystroke.key, m.shift)
        {
            let (groups, _) = self.groups(cx);
            let entry = groups
                .into_iter()
                .flat_map(|g| g.entries)
                .nth(number - 1)
                .map(|e| (e.key, e.action, e.remember));
            if let Some(entry) = entry {
                cx.stop_propagation();
                if !event.is_held {
                    self.run(entry, window, cx);
                }
            }
            return;
        }
        if keystroke.key == "escape" {
            cx.stop_propagation();
            self.close(window, cx);
            return;
        }
        if m.control || m.alt || m.platform {
            return;
        }
        match keystroke.key.as_str() {
            "down" | "up" => {
                if self.highlight.step(keystroke.key == "down", keys) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }
            "enter" => {
                let Some(chosen) = self.highlight.chosen(keys) else {
                    return;
                };
                cx.stop_propagation();
                let (groups, _) = self.groups(cx);
                let entry = groups
                    .into_iter()
                    .flat_map(|g| g.entries)
                    .find(|e| e.key == chosen)
                    .map(|e| (e.key, e.action, e.remember));
                if let Some(entry) = entry {
                    self.run(entry, window, cx);
                }
            }
            _ => {}
        }
    }

    fn person_avatar(&self, leading: &EntryLeading, cx: &App) -> Option<AnyElement> {
        let EntryLeading::Person {
            pubkey,
            name,
            picture,
            agent,
        } = leading
        else {
            return None;
        };
        let community = self.community.as_ref()?.read(cx);
        let media = community.session.media();
        let picture = community
            .snapshot
            .profiles
            .get(pubkey)
            .and_then(|p| p.picture.clone())
            .or_else(|| picture.clone());
        let mut avatar = Avatar::new(name.clone()).size(AvatarSize::Px(20.));
        if *agent {
            avatar = avatar.squircle();
        }
        if let Some(picture) = picture {
            avatar = avatar.image(super::media::avatar(&picture, media));
        }
        Some(avatar.into_any_element())
    }

    fn status_area(&self, status: Vec<Status>, cx: &mut Context<Self>) -> Option<Div> {
        if status.is_empty() {
            return None;
        }
        let c = *cx.colors();
        let mut area = div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .px(rems(1.))
            .pt(rems(0.5))
            .text_body_sm()
            .text_color(c.text_subtle);
        for (n, item) in status.into_iter().enumerate() {
            area = area.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(item.text)
                    .when_some(item.retry, |this, (label, retry)| {
                        this.child(
                            Button::new(SharedString::from(format!("search-retry-{n}")), label)
                                .ghost()
                                .size(ButtonSize::Sm)
                                .surface(Surface::Floating)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.retry(retry, window, cx)
                                })),
                        )
                    }),
            );
        }
        Some(area)
    }
}

impl Render for SearchDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let (groups, status) = self.groups(cx);
        let groups: Vec<Group> = groups
            .into_iter()
            .filter(|g| !g.entries.is_empty() || g.empty.is_some())
            .collect();
        let keys: Rc<Vec<String>> = Rc::new(
            groups
                .iter()
                .flat_map(|g| g.entries.iter().map(|e| e.key.clone()))
                .collect(),
        );
        let query = self.query(cx);
        self.highlight.sync(&query, &keys);
        let active = self.highlight.active().map(str::to_owned);

        // Rows are direct children of the scrolled list, so the highlight
        // can scroll one into view.
        let mut children: Vec<AnyElement> = Vec::new();
        let mut index_of: HashMap<String, usize> = HashMap::new();
        for (n, group) in groups.into_iter().enumerate() {
            children.push(
                div()
                    .when(n > 0, |this| {
                        this.mt(rems(0.75))
                            .border_t_1()
                            .border_color(c.border_standard)
                            .pt(rems(0.5))
                    })
                    .child(group_label(group.label, cx))
                    .into_any_element(),
            );
            let empty = group.entries.is_empty().then_some(group.empty).flatten();
            for entry in group.entries {
                let image = match &entry.action {
                    Action::Page(page) => pages::image(*page, cx),
                    _ => None,
                };
                let leading = match (&entry.leading, image) {
                    (EntryLeading::Icon(_), Some(image)) => Leading::Element(image),
                    (EntryLeading::Icon(icon), None) => Leading::Icon(*icon),
                    (person, _) => match self.person_avatar(person, cx) {
                        Some(avatar) => Leading::Element(avatar),
                        None => Leading::Icon(IconName::ChatCircle),
                    },
                };
                let run = (entry.key.clone(), entry.action.clone(), entry.remember);
                let hover_key = entry.key.clone();
                let hover_keys = keys.clone();
                index_of.insert(entry.key.clone(), children.len());
                children.push(
                    labeled_row(
                        SharedString::from(format!("search-{}", entry.key)),
                        leading,
                        search_kit::matched_label(entry.label, &entry.positions),
                        entry.detail,
                        active.as_deref() == Some(entry.key.as_str()),
                        cx,
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if this
                            .highlight
                            .hover(&hover_key, event.position, &hover_keys)
                        {
                            cx.notify();
                        }
                    }))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.run(run.clone(), window, cx)),
                    )
                    .into_any_element(),
                );
            }
            if let Some(empty) = empty {
                children.push(note(empty, cx).into_any_element());
            }
        }
        if let Some(area) = self.status_area(status, cx) {
            children.push(area.into_any_element());
        }
        self.highlight.reveal(|key| index_of.get(key).copied());

        let top = (window.viewport_size().height * 0.17).clamp(px(48.), px(160.));
        let width = (window.viewport_size().width - px(32.)).min(px(640.));
        let title = if self.scope.is_some() {
            "Search this conversation"
        } else {
            "Search Buzz"
        };
        let scope = self.scope.as_ref().map(|channel| {
            let ready = self.plan(cx).is_some();
            let (icon, label) = match self.community.as_ref().filter(|_| ready) {
                Some(community) => (
                    Some(channel_icon(channel)),
                    channel_label(channel, &community.read(cx).snapshot.profiles),
                ),
                None => (None, "This conversation".into()),
            };
            div().flex().child(
                div()
                    .id("search-scope")
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .mb(rems(0.5))
                    .px(rems(0.5))
                    .py(rems(0.25))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(c.border_standard)
                    .bg(c.affordance_subtle)
                    .text_body_sm()
                    .cursor_pointer()
                    .children(icon.map(|icon| Icon::new(icon).size(14.)))
                    .child(label)
                    .child(Icon::new(IconName::X).size(14.))
                    .on_click(cx.listener(|this, _, window, cx| this.unscope(window, cx))),
            )
        });
        let key_list = keys.clone();
        div()
            .id("search-backdrop")
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(top)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close(window, cx)),
            )
            .child(
                div()
                    .id("search-palette")
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .flex()
                    .flex_col()
                    .w(width)
                    .max_h(window.viewport_size().height - top - px(16.))
                    .p(rems(1.5))
                    .bg(c.surface_popover)
                    .border_1()
                    .border_color(c.border_standard)
                    .rounded(buzz_ui::tokens::radius::PANEL)
                    .shadow(shadow::sm())
                    .text_color(c.text_standard)
                    .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        this.key_down(event, &key_list, window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .justify_between()
                            .child(div().text_label().child(title))
                            .child(
                                IconButton::new("search-close", IconName::X)
                                    .size(IconButtonSize::Sm)
                                    .surface(Surface::Floating)
                                    .tooltip("Close search")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.close(window, cx)),
                                    ),
                            ),
                    )
                    .child(div().pt(rems(1.5)).children(scope).child(self.field(cx)))
                    .child(
                        div()
                            .id("search-results")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .max_h(window.viewport_size().height * 0.55)
                            .overflow_y_scroll()
                            .track_scroll(self.highlight.scroll_handle())
                            .mt(rems(0.5))
                            .mx(rems(-0.5))
                            .px(rems(0.5))
                            .py(rems(0.25))
                            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                                this.highlight.track_pointer(event.position)
                            }))
                            .children(children),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn recognizes_unfinished_date_tokens() {
        assert_eq!(
            date_prompt("deploy after:"),
            Some(("deploy ".to_string(), "after"))
        );
        assert_eq!(date_prompt("BEFORE:20"), Some((String::new(), "before")));
        assert_eq!(date_prompt("after:2026-01-02"), None);
        assert_eq!(date_prompt("in:general"), None);
    }

    #[test]
    fn date_presets_start_weeks_on_monday() {
        let thursday = NaiveDate::from_ymd_opt(2026, 10, 8).unwrap();
        let presets = date_presets(thursday);
        assert_eq!(presets[1].1, NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());
        assert_eq!(presets[2].1, NaiveDate::from_ymd_opt(2026, 10, 5).unwrap());
        assert_eq!(presets[3].1, NaiveDate::from_ymd_opt(2026, 9, 28).unwrap());
        assert_eq!(presets[4].1, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
    }

    #[test]
    fn page_names_match_by_prefix() {
        assert_eq!(match_rank("Settings", "set"), Some(1));
    }

    #[test]
    fn picked_authors_replace_their_token() {
        // A name still being typed at the end.
        assert_eq!(author_display("deploy from:@ali", true), "deploy ");
        assert_eq!(author_display("from:", true), "");
        // A completed name that matched several people, with text after it.
        assert_eq!(
            author_display("deploy from:alice in:general", false),
            "deploy in:general"
        );
        assert_eq!(author_display("FROM:@bob", false), "");
        assert_eq!(author_display("no author", false), "no author");
        // Tokens whose fifth byte falls inside a character.
        assert_eq!(author_display("from:alex 日本語", false), "日本語");
        assert_eq!(author_display("日本 from:al", true), "日本 ");
    }

    #[test]
    fn errors_read_like_buzz_app() {
        assert_eq!(
            message_error("timed out", false),
            "Message search couldn’t finish: timed out Pages and conversations are still available."
        );
        assert_eq!(
            message_error("", true),
            "Message search couldn’t finish. Try again."
        );
        assert_eq!(
            public_error("closed"),
            "Public channel search couldn’t finish: closed"
        );
        assert_eq!(public_error(""), "Public channel search couldn’t finish.");
    }

    #[test]
    fn result_shortcuts_follow_the_physical_digit() {
        assert_eq!(shortcut_number("1", true), Some(1));
        assert_eq!(shortcut_number("9", true), Some(9));
        // Without Shift a digit is not the shortcut.
        assert_eq!(shortcut_number("1", false), None);
        // Linux reports the shifted symbol and consumes Shift.
        assert_eq!(shortcut_number("!", false), Some(1));
        assert_eq!(shortcut_number("(", true), Some(9));
        assert_eq!(shortcut_number("0", true), None);
        assert_eq!(shortcut_number("a", true), None);
    }

    #[test]
    fn lookups_answer_only_their_request() {
        let mut lookup: Lookup<u8> = Lookup::default();
        let request = lookup.wants(Some("a")).expect("a new read");
        lookup.pending = Some(request.clone());
        // Already in flight.
        assert_eq!(lookup.wants(Some("a")), None);
        lookup.key = Some(request);
        lookup.value = Some(1);
        lookup.pending = None;
        assert_eq!(lookup.current("a"), Some(&1));
        assert_eq!(lookup.current("b"), None);
        assert_eq!(lookup.last(), Some(&1));
        // A retry asks again.
        lookup.attempt += 1;
        assert_eq!(lookup.current("a"), None);
        assert!(lookup.wants(Some("a")).is_some());
    }
}
