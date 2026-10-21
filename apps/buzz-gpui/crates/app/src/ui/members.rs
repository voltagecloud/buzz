//! Channel members (ui.md §4.2, `ChannelMembersDialog.tsx`,
//! `MemberAdministration.tsx`): the roster grouped by role with presence
//! and a role filter, adding people and agents, each member's actions
//! (profile, direct message), and role changes and removal for channel
//! admins and for an agent's verified owner. Searching also offers the
//! owner's Agents teams, each added with one button.

use super::message::display_name;
use super::modal::{Dismissed, error, group_label, note};
use super::navigation;
use super::overlay::{self, Align, Dismiss};
use crate::agents2::teams::Team;
use crate::community::Community;
use buzz_relay_client::lifecycle::{ChannelState, MemberRole};
use buzz_relay_client::members::{self, Change, MemberChange, Person};
use buzz_relay_client::{
    ChannelSummary, ChannelType, MemberAddition, MemberOperation, MemberOperationStatus,
    PeoplePage, PresenceState, Profiles,
};
use buzz_ui::{
    ActiveTheme, AlertDialog, Avatar, AvatarSize, Button, ButtonSize, ButtonVariant, Dialog,
    IconButton, IconButtonSize, IconName, Menu, MenuItem, MenuSeparator, Presence, SearchField,
    Spinner, Surface, TypeStyled,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(180);

/// The roster's sections, in order, and the role filter's options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Owners,
    Admins,
    Members,
    Agents,
}

impl Group {
    const ALL: [Group; 4] = [Self::Owners, Self::Admins, Self::Members, Self::Agents];

    fn label(self) -> &'static str {
        match self {
            Self::Owners => "Owners",
            Self::Admins => "Admins",
            Self::Members => "Members",
            Self::Agents => "Agents",
        }
    }

    /// Owners and admins by role first; then agents; everyone else is a
    /// member.
    fn of(key: &str, role: MemberRole, profiles: &Profiles, known: &HashSet<String>) -> Self {
        match role {
            MemberRole::Owner => Self::Owners,
            MemberRole::Admin => Self::Admins,
            _ if is_agent(key, role, profiles, known) => Self::Agents,
            _ => Self::Members,
        }
    }
}

/// A bot by role, a known agent of the viewer's library, or an agent by
/// its signed profile (`useKnownAgents` plus `isAgent`).
fn is_agent(key: &str, role: MemberRole, profiles: &Profiles, known: &HashSet<String>) -> bool {
    role == MemberRole::Bot || known.contains(key) || profiles.get(key).is_some_and(|p| p.is_agent)
}

/// The viewer's library agents, by key.
fn known_agents(cx: &App) -> HashSet<String> {
    super::identity::AgentLibrary::from_hosts(cx).keys()
}

/// The role a row announces (`roleLabel`): an agent's bot role reads as
/// member, and a member the roster could not verify says so.
fn role_label(role: Option<MemberRole>, agent: bool) -> &'static str {
    match role {
        Some(MemberRole::Bot) if agent => "member",
        Some(MemberRole::Owner) => "owner",
        Some(MemberRole::Admin) => "admin",
        Some(MemberRole::Member) => "member",
        Some(MemberRole::Guest) => "guest",
        Some(MemberRole::Bot) => "bot",
        Some(MemberRole::Unknown) | None => "Role unverified",
    }
}

/// The search results shown per page (`MEMBER_SEARCH_PAGE_SIZE`).
const SEARCH_PAGE: usize = 30;

/// The teams the search offers (`ChannelMembersDialog.tsx`): those whose
/// name matches `query`, each with its members that are the owner's
/// agents in this community (`mine`) and not yet in the channel. A team
/// with none of them is not offered.
fn team_offers<'a>(
    teams: &'a [Team],
    query: &str,
    mine: impl Fn(&str) -> bool,
    member: impl Fn(&str) -> bool,
) -> Vec<(&'a Team, Vec<String>)> {
    teams
        .iter()
        .filter_map(|team| {
            let missing: Vec<String> = team
                .members
                .iter()
                .filter(|key| mine(key) && !member(key))
                .cloned()
                .collect();
            (!missing.is_empty() && members::match_tier(&team.name, query).is_some())
                .then_some((team, missing))
        })
        .collect()
}

/// An agent's owner, when its attestation verifies.
fn owner_of(key: &str, profiles: &Profiles) -> Option<String> {
    profiles.get(key)?.attested_owner.clone()
}

pub struct MembersDialog {
    community: Entity<Community>,
    channel: ChannelSummary,
    state: Option<Result<ChannelState, String>>,
    loading: bool,
    input: Entity<InputState>,
    /// People found for the typed text, with the text they answer.
    found: Option<(String, Result<PeoplePage, String>)>,
    /// Relay pages read for the found text, and how many results show.
    found_pages: usize,
    shown: usize,
    searching: Option<Task<()>>,
    /// "Some names could not load."
    names_failed: bool,
    /// "Some agent managers or their names could not load."
    owners_failed: bool,
    /// People being added, changed or messaged.
    busy: HashSet<String>,
    error: Option<SharedString>,
    /// Additions the session tracks here: in flight, or failed.
    additions: Vec<MemberAddition>,
    /// "<name> is in the channel."
    notice: Option<SharedString>,
    /// "Send message" is opening the conversation.
    opening: bool,
    /// The latest role change or removal here.
    operation: Option<MemberOperation>,
    menu: Option<String>,
    confirm: Option<MemberChange>,
    /// `None` shows every group.
    filter: Option<Group>,
    filter_open: bool,
    _subscription: Subscription,
    _agents: Option<Subscription>,
}

impl EventEmitter<Dismissed> for MembersDialog {}

impl MembersDialog {
    pub fn new(
        community: Entity<Community>,
        channel: ChannelSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search members"));
        input.update(cx, |state, cx| state.focus(window, cx));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    // Typing searches everyone again.
                    this.filter = None;
                    this.filter_open = false;
                    this.search(window, cx);
                }
            });
        let mut dialog = Self {
            community,
            channel,
            state: None,
            loading: false,
            input,
            found: None,
            found_pages: 0,
            shown: SEARCH_PAGE,
            searching: None,
            names_failed: false,
            owners_failed: false,
            busy: HashSet::new(),
            error: None,
            additions: Vec::new(),
            notice: None,
            opening: false,
            operation: None,
            menu: None,
            confirm: None,
            filter: None,
            filter_open: false,
            _subscription: subscription,
            // Shows the agent list failing or reading again.
            _agents: crate::agents2::Agents2::global(cx)
                .map(|agents| cx.observe(&agents, |_, _, cx| cx.notify())),
        };
        dialog.refresh(window, cx);
        super::archives::ensure(&dialog.community, false, cx);
        dialog
    }

    fn viewer(&self, cx: &App) -> String {
        self.community.read(cx).session.viewer().to_string()
    }

    /// Reads the additions and the role change the session tracks for
    /// this channel, which outlive the dialog.
    fn read_tracked(&mut self, cx: &mut Context<Self>) {
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.id.clone();
        cx.spawn(async move |this, cx| {
            let additions = session.member_additions(&channel).await;
            let operation = session.member_operation(&channel).await;
            let _ = this.update(cx, |this, cx| {
                this.additions = additions;
                this.operation = operation;
                cx.notify();
            });
        })
        .detach();
    }

    /// "Refresh member data": the roster and roles again, which also
    /// settles an uncertain role change the roster now shows, and the
    /// viewer's agents from their hosts.
    fn refresh_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || !self.busy.is_empty() {
            return;
        }
        self.error = None;
        if let Some(agents) = crate::agents2::Agents2::global(cx) {
            agents.update(cx, |agents, cx| agents.refresh(cx).detach());
        }
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.refresh_members(&channel).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    this.error = Some(error.into());
                }
                this.refresh(window, cx);
            });
        })
        .detach();
    }

    /// Re-reads the roster and roles from the relay.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.read_tracked(cx);
        self.loading = true;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let state = session.channel_state(&channel).await;
            let Ok(people) = this.update_in(cx, |this, window, cx| {
                // Without a confirmed roster the sidebar's members still
                // show, their roles unverified.
                let people: Vec<String> = match &state {
                    Ok(state) => state.roles.keys().cloned().collect(),
                    Err(_) => this.summary(cx).members,
                };
                this.community.update(cx, |community, _| {
                    community.set_presence_extra(people.clone());
                });
                this.state = Some(state);
                this.loading = false;
                this.update_placeholder(window, cx);
                cx.notify();
                people
            }) else {
                return;
            };
            let names = session.load_profiles(people.clone()).await;
            let owners: Vec<String> = session
                .snapshot()
                .borrow()
                .profiles
                .iter()
                .filter(|(key, _)| people.contains(key))
                .filter_map(|(_, profile)| profile.attested_owner.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let owners = session.load_profiles(owners).await;
            let _ = this.update(cx, |this, cx| {
                this.names_failed = names.is_err();
                this.owners_failed = owners.is_err();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn update_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = if self.can_add(cx) {
            "Add people and agents"
        } else {
            "Search members"
        };
        self.input.update(cx, |state, cx| {
            state.set_placeholder(placeholder, window, cx);
        });
    }

    fn can_add(&self, cx: &App) -> bool {
        let viewer = self.viewer(cx);
        let member = matches!(&self.state, Some(Ok(state)) if state.role(&viewer).is_some());
        let summary = self.summary(cx);
        members::can_add_members(&summary, member)
    }

    /// The channel as the sidebar knows it now.
    fn summary(&self, cx: &App) -> ChannelSummary {
        self.community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel.id)
            .cloned()
            .unwrap_or_else(|| self.channel.clone())
    }

    fn search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().trim().to_string();
        if query.is_empty() || !self.can_add(cx) {
            self.searching = None;
            self.found = None;
            cx.notify();
            return;
        }
        let session = self.community.read(cx).session.clone();
        self.shown = SEARCH_PAGE;
        self.searching = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let page = session.search_members(&query, 1).await;
            let _ = this.update(cx, |this, cx| {
                this.found = Some((query, page));
                this.found_pages = 1;
                this.searching = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// "Show more results": the next bound of results, reading the relay's
    /// next page once the ones already read are all shown.
    fn show_more(&mut self, available: usize, window: &mut Window, cx: &mut Context<Self>) {
        let more_here = available > self.shown;
        self.shown += SEARCH_PAGE;
        if more_here {
            cx.notify();
            return;
        }
        let Some((query, Ok(_))) = self.found.clone() else {
            return;
        };
        let session = self.community.read(cx).session.clone();
        let page = self.found_pages + 1;
        self.searching = Some(cx.spawn_in(window, async move |this, cx| {
            let next = session.search_members(&query, page).await;
            let _ = this.update(cx, |this, cx| {
                this.searching = None;
                match (next, &mut this.found) {
                    (Ok(next), Some((text, Ok(found)))) if *text == query => {
                        for person in next.people {
                            if !found.people.iter().any(|p| p.pubkey == person.pubkey) {
                                found.people.push(person);
                            }
                        }
                        found.more = next.more;
                        this.found_pages = page;
                    }
                    (Err(error), Some((text, found))) if *text == query => *found = Err(error),
                    _ => {}
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// "Add team": adds each of `missing` through the normal additions,
    /// together. A failed member shows its own error and Retry.
    fn add_team(
        &mut self,
        team: String,
        missing: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if missing.iter().any(|key| self.is_busy(key)) {
            return;
        }
        self.notice = None;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.id.clone();
        for key in &missing {
            self.busy.insert(key.clone());
        }
        self.additions
            .retain(|addition| !missing.contains(&addition.pubkey));
        cx.spawn_in(window, async move |this, cx| {
            let added = futures::future::join_all(
                missing.iter().map(|key| session.add_member(&channel, key)),
            )
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                for key in &missing {
                    this.busy.remove(key);
                }
                if added.iter().all(Result::is_ok) {
                    this.notice = Some(format!("{team} is in the channel.").into());
                }
                this.refresh(window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// A team of the owner's agents, added with one button (`TeamRow`).
    fn team_row(&self, team: &Team, missing: &[String], cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let busy = missing.iter().any(|key| self.is_busy(key));
        let community = self.community.read(cx);
        let agents = crate::agents2::Agents2::global(cx);
        let names: Vec<SharedString> = missing
            .iter()
            .map(|key| {
                match community.snapshot.profiles.get(key) {
                    Some(profile) => Some(profile.name.clone()),
                    None => agents
                        .as_ref()
                        .and_then(|agents| agents.read(cx).find(key))
                        .map(|agent| agent.name.clone()),
                }
                .map(SharedString::from)
                .unwrap_or_else(|| display_name(&community.snapshot.profiles, key))
            })
            .collect();
        let (name, id) = (team.name.clone(), team.id.clone());
        let add = missing.to_vec();
        div()
            .id(SharedString::from(format!("team-{id}")))
            .flex()
            .items_center()
            .gap(rems(0.75))
            .min_h(rems(2.75))
            .px(rems(1.))
            .py(rems(0.25))
            .rounded(px(11.))
            .hover(|s| s.bg(c.interaction(Surface::Floating).row_hover))
            .child(Avatar::new(name.clone()).size(AvatarSize::Small).squircle())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_body_sm().truncate().child(name.clone()))
                    .child(
                        div()
                            .text_caption()
                            .text_color(c.text_subtle)
                            .truncate()
                            .child(names.join(", ")),
                    ),
            )
            .child(
                Button::new(
                    SharedString::from(format!("add-team-{id}")),
                    if busy { "Adding…" } else { "Add team" },
                )
                .prominent()
                .size(ButtonSize::Xs)
                .disabled(self.loading)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.add_team(name.clone(), add.clone(), window, cx)
                })),
            )
            .into_any_element()
    }

    /// Adds someone (`memberAdditions.add`). A failure stays with the
    /// person, with Retry.
    fn add(&mut self, pubkey: String, window: &mut Window, cx: &mut Context<Self>) {
        if super::archives::hides(&self.community, &pubkey, cx) {
            self.error = Some("Archived identities cannot be added. Choose someone else.".into());
            cx.notify();
            return;
        }
        if !self.busy.insert(pubkey.clone()) {
            return;
        }
        self.notice = None;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.id.clone();
        self.additions.retain(|addition| addition.pubkey != pubkey);
        cx.spawn_in(window, async move |this, cx| {
            let added = session.add_member(&channel, &pubkey).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy.remove(&pubkey);
                if added.is_ok() {
                    let name = display_name(&this.community.read(cx).snapshot.profiles, &pubkey);
                    this.notice = Some(format!("{name} is in the channel.").into());
                }
                this.refresh(window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(change) = self.confirm.take() else {
            return;
        };
        let pubkey = change.pubkey.clone();
        self.busy.insert(pubkey.clone());
        self.error = None;
        let session = self.community.read(cx).session.clone();
        let channel = self.channel.id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.change_member(&channel, change).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy.remove(&pubkey);
                match result {
                    Ok(()) => this.refresh(window, cx),
                    Err(error) => {
                        this.error = Some(error.into());
                        this.read_tracked(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// "Send message": opens (or reopens and unhides) the DM with this
    /// person, then shows it and closes the dialog.
    fn message(&mut self, pubkey: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.busy.insert(pubkey.clone()) {
            return;
        }
        self.error = None;
        self.opening = true;
        let session = self.community.read(cx).session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.open_dm(vec![pubkey.clone()]).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy.remove(&pubkey);
                this.opening = false;
                match result {
                    Ok(id) => {
                        navigation::open_conversation(id, window, cx);
                        cx.emit(Dismissed);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not open the direct message");
                        this.error =
                            Some("Could not open the conversation. Try Send message again.".into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn dismiss_menu(cx: &mut Context<Self>) -> Dismiss {
        let weak = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.menu = None;
                this.filter_open = false;
                cx.notify();
            });
        })
    }

    fn avatar(&self, pubkey: &str, agent: bool, size: f32, cx: &App) -> Avatar {
        let community = self.community.read(cx);
        let snapshot = &community.snapshot;
        let mut avatar = Avatar::new(display_name(&snapshot.profiles, pubkey))
            .size(AvatarSize::Px(size))
            .presence(snapshot.presence.get(pubkey).map(|state| match state {
                PresenceState::Online => Presence::Online,
                PresenceState::Away => Presence::Away,
                PresenceState::Offline => Presence::Offline,
            }));
        if agent {
            avatar = avatar.squircle();
        }
        if let Some(picture) = snapshot
            .profiles
            .get(pubkey)
            .and_then(|p| p.picture.clone())
        {
            avatar = avatar.image(super::media::avatar(&picture, community.session.media()));
        }
        avatar
    }

    /// The row's actions (`MemberAdministration.tsx`): profiles and a DM
    /// for anyone, role changes for admins, removal for admins and for an
    /// agent's verified owner.
    fn actions_menu(
        &self,
        pubkey: &str,
        role: MemberRole,
        state: &ChannelState,
        cx: &mut Context<Self>,
    ) -> Menu {
        let viewer = self.viewer(cx);
        let profiles = self.community.read(cx).snapshot.profiles.clone();
        let agent = is_agent(pubkey, role, &profiles, &known_agents(cx));
        let owner = owner_of(pubkey, &profiles);
        let key = pubkey.to_string();
        let profile_item = |id: &'static str, label: &'static str, target: String| {
            MenuItem::new(id, label).on_click(cx.listener(
                move |this, event: &ClickEvent, window, cx| {
                    this.menu = None;
                    navigation::show_profile(target.clone(), event.position(), window, cx);
                    cx.emit(Dismissed);
                },
            ))
        };
        let mut menu = Menu::new().child(profile_item("view-profile", "View profile", key.clone()));
        if let Some(owner) = owner.clone() {
            menu = menu.child(profile_item(
                "view-owner-profile",
                "View owner profile",
                owner,
            ));
        }
        if !agent && pubkey != viewer {
            let target = key.clone();
            menu = menu.child(
                MenuItem::new("send-message", "Send message").on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.menu = None;
                        this.message(target.clone(), window, cx);
                    },
                )),
            );
        }
        let change = |change: Change| MemberChange {
            pubkey: key.clone(),
            expected: role,
            change,
        };
        let confirm = |id: &'static str, label: &'static str, next: MemberChange| {
            MenuItem::new(id, label).on_click(cx.listener(move |this, _, _, cx| {
                this.menu = None;
                this.confirm = Some(next.clone());
                cx.notify();
            }))
        };
        // A change in flight, or one that may have taken effect, blocks
        // the next until a refresh settles it.
        let settled = !self.operation.as_ref().is_some_and(|operation| {
            matches!(
                operation.status,
                MemberOperationStatus::Pending | MemberOperationStatus::Uncertain
            )
        });
        let can_edit = settled && state.can_manage_member(&viewer, pubkey);
        let can_remove = settled && state.can_remove_member(&viewer, pubkey, owner.as_deref());
        if can_edit {
            menu = menu.child(MenuSeparator::new());
            for (id, next, target, label) in [
                ("make-admin", Change::Admin, MemberRole::Admin, "Make admin"),
                (
                    "make-member",
                    Change::Member,
                    MemberRole::Member,
                    "Make member",
                ),
            ] {
                if target != role {
                    menu = menu.child(confirm(id, label, change(next)));
                }
            }
        }
        if can_remove {
            menu = menu
                .child(MenuSeparator::new())
                .child(confirm("remove", "Remove from channel", change(Change::Remove)).danger());
        }
        menu
    }

    /// A member's row. Without `state` the roster is unconfirmed: the
    /// row announces its role as unverified and offers no actions.
    #[allow(clippy::too_many_arguments)]
    fn member_row(
        &self,
        pubkey: &str,
        role: MemberRole,
        state: Option<&ChannelState>,
        key_label: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let viewer = self.viewer(cx);
        let profiles = self.community.read(cx).snapshot.profiles.clone();
        let agent = is_agent(pubkey, role, &profiles, &known_agents(cx));
        let label = format!(
            "Open profile for {} ({key_label}){}, {}{}",
            display_name(&profiles, pubkey),
            if agent { ", agent" } else { "" },
            role_label(state.map(|_| role), agent),
            if super::archives::hides(&self.community, pubkey, cx) {
                ", archived"
            } else {
                ""
            },
        );
        let mut name = display_name(&profiles, pubkey).to_string();
        if pubkey == viewer {
            name.push_str(" (you)");
        }
        // "managed by …" for an agent whose owner is verified.
        let managed = agent
            .then(|| owner_of(pubkey, &profiles))
            .flatten()
            .map(|owner| {
                let mut owner_name = display_name(&profiles, &owner).to_string();
                if owner == viewer {
                    owner_name.push_str(" (you)");
                }
                format!("managed by {owner_name}")
            });
        let menu_open = self.menu.as_deref() == Some(pubkey);
        let busy = self.busy.contains(pubkey);
        let key = pubkey.to_string();
        let menu = state.filter(|_| menu_open).map(|state| {
            let menu = self.actions_menu(pubkey, role, state, cx);
            overlay::dropdown(window, Align::End, menu, Self::dismiss_menu(cx))
        });
        let id = SharedString::from(format!("member-{pubkey}"));
        div()
            .id(id)
            .aria_label(label)
            .group("member-row")
            .flex()
            .items_center()
            .gap(rems(0.75))
            .min_h(rems(2.75))
            .px(rems(1.))
            .py(rems(0.25))
            .rounded(px(11.))
            .hover(|s| s.bg(c.interaction(Surface::Floating).row_hover))
            .child(self.avatar(pubkey, agent, 32., cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_body_sm().truncate().child(name))
                    .children(managed.map(|managed| {
                        div()
                            .text_caption()
                            .text_color(c.text_subtle)
                            .truncate()
                            .child(managed)
                    })),
            )
            .when(busy, |this| this.child(Spinner::new().size(16.)))
            .when(!busy && state.is_some(), |this| {
                this.child(
                    div()
                        .relative()
                        .child(
                            IconButton::new(
                                SharedString::from(format!("member-actions-{pubkey}")),
                                IconName::DotsThree,
                            )
                            .size(IconButtonSize::Sm)
                            .surface(Surface::Floating)
                            .when(!menu_open, |b| {
                                b.tooltip(format!(
                                    "Actions for {}",
                                    display_name(&profiles, pubkey)
                                ))
                            })
                            .on_click(cx.listener({
                                let key = key.clone();
                                move |this, _, _, cx| {
                                    this.menu = Some(key.clone());
                                    cx.notify();
                                }
                            })),
                        )
                        .children(menu),
                )
            })
            .into_any_element()
    }

    /// Someone being added here, or a change or message in flight.
    fn is_busy(&self, pubkey: &str) -> bool {
        self.busy.contains(pubkey)
            || self
                .additions
                .iter()
                .any(|addition| addition.pubkey == pubkey && addition.pending)
    }

    /// Each failed addition with its error, as buzz-app lists them.
    fn addition_errors(&self) -> Vec<(String, String)> {
        let mut errors: Vec<(String, String)> = self
            .additions
            .iter()
            .filter(|addition| !self.busy.contains(&addition.pubkey))
            .filter_map(|addition| Some((addition.pubkey.clone(), addition.error.clone()?)))
            .collect();
        errors.sort();
        errors
    }

    fn candidate_row(&self, person: &Person, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let busy = self.is_busy(&person.pubkey);
        let key = person.pubkey.clone();
        div()
            .id(SharedString::from(format!("candidate-{}", person.pubkey)))
            .flex()
            .items_center()
            .gap(rems(0.75))
            .min_h(rems(2.75))
            .px(rems(1.))
            .py(rems(0.25))
            .rounded(px(11.))
            .hover(|s| s.bg(c.interaction(Surface::Floating).row_hover))
            .child(self.avatar(&person.pubkey, person.profile.is_agent, 32., cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_body_sm()
                    .truncate()
                    .child(person.profile.name.clone()),
            )
            .child(
                Button::new(
                    SharedString::from(format!("add-{}", person.pubkey)),
                    if busy { "Adding…" } else { "Add" },
                )
                .prominent()
                .size(ButtonSize::Xs)
                .disabled(busy)
                .on_click(
                    cx.listener(move |this, _, window, cx| this.add(key.clone(), window, cx)),
                ),
            )
            .into_any_element()
    }

    fn confirmation(&self, change: &MemberChange, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let profiles = self.community.read(cx).snapshot.profiles.clone();
        let name = display_name(&profiles, &change.pubkey);
        let cancel = Button::new("member-cancel", "Cancel")
            .variant(ButtonVariant::Subtle)
            .surface(Surface::Floating)
            .on_click(cx.listener(|this, _, _, cx| {
                this.confirm = None;
                cx.notify();
            }));
        let confirm = |label: String| {
            Button::new("member-confirm", label)
                .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx)))
        };
        if change.change == Change::Remove {
            let agent = is_agent(
                &change.pubkey,
                change.expected,
                &profiles,
                &known_agents(cx),
            );
            let noun = if agent { "agent" } else { "member" };
            return AlertDialog::new(format!("Remove {noun} from channel"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rems(0.75))
                        .child(self.avatar(&change.pubkey, agent, 32., cx))
                        .child(
                            div()
                                .text_body()
                                .text_color(c.text_standard)
                                .truncate()
                                .child(name),
                        ),
                )
                .action(cancel)
                .action(confirm(format!("Remove {noun}")).destructive())
                .into_any_element();
        }
        let role = if change.change == Change::Admin {
            "admin"
        } else {
            "member"
        };
        AlertDialog::new("Change member role?")
            .description(format!(
                "Change this member’s role from {} to {role}. {}",
                change.expected.as_str(),
                if change.change == Change::Admin {
                    "Admins can manage this channel and its members."
                } else {
                    "This changes their authority in this channel."
                }
            ))
            .action(cancel)
            .action(confirm(format!("Make {role}")).prominent())
            .into_any_element()
    }

    /// "Filter members by role": a compact select of the groups present.
    fn filter_select(
        &self,
        counts: &[(Group, usize)],
        total: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = |group: Option<Group>| match group {
            None => format!("All · {total}"),
            Some(group) => {
                let count = counts
                    .iter()
                    .find(|(g, _)| *g == group)
                    .map_or(0, |(_, n)| *n);
                format!("{} · {count}", group.label())
            }
        };
        let menu = self.filter_open.then(|| {
            let mut menu = Menu::new();
            let options = std::iter::once(None).chain(counts.iter().map(|(group, _)| Some(*group)));
            for (index, option) in options.enumerate() {
                menu = menu.child(
                    MenuItem::new(("member-filter", index), label(option))
                        .checked(self.filter == option)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.filter = option;
                            this.filter_open = false;
                            cx.notify();
                        })),
                );
            }
            overlay::dropdown(window, Align::End, menu, Self::dismiss_menu(cx))
        });
        div()
            .relative()
            .flex_none()
            .child(
                Button::new("member-filter", label(self.filter))
                    .compact_select()
                    .size(ButtonSize::Sm)
                    .surface(Surface::Floating)
                    .icon_end(if self.filter_open {
                        IconName::CaretUp
                    } else {
                        IconName::CaretDown
                    })
                    .when(!self.filter_open, |b| b.tooltip("Filter members by role"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.filter_open = !this.filter_open;
                        cx.notify();
                    })),
            )
            .children(menu)
            .into_any_element()
    }
}

impl Render for MembersDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(change) = self.confirm.clone() {
            return self.confirmation(&change, cx);
        }
        let c = *cx.colors();
        let viewer = self.viewer(cx);
        let summary = self.summary(cx);
        let profiles = self.community.read(cx).snapshot.profiles.clone();
        let query = self.input.read(cx).value().trim().to_lowercase();
        let can_add = self.can_add(cx);
        let known = known_agents(cx);

        let mut list = div().flex().flex_col().gap(rems(0.75));
        if !can_add {
            let reason = if summary.channel_type == Some(ChannelType::Dm) {
                "DM membership cannot be changed here."
            } else if summary.archived {
                "Archived channels cannot add members."
            } else if summary.cached {
                "Adding members is unavailable in this connection."
            } else {
                "Join this channel to add people and agents."
            };
            list = list.child(note(reason, cx));
        }
        if let Some(Err(message)) = &self.state {
            list = list.child(div().px(rems(1.)).child(error(message.clone(), cx)));
        }
        let operation_error = self
            .operation
            .as_ref()
            .filter(|operation| operation.status == MemberOperationStatus::Uncertain)
            .and_then(|operation| operation.error.clone());
        if let Some(message) = self.error.clone().or(operation_error.map(Into::into)) {
            list = list.child(div().px(rems(1.)).child(error(message, cx)));
        }
        if super::archives::status(&self.community, cx) == Some(super::archives::Status::Error) {
            list = list.child(
                div()
                    .px(rems(1.))
                    .child(error("Archived identities could not be checked.", cx)),
            );
        }
        let mut filter = None;
        match &self.state {
            None => {
                list = list.child(
                    div()
                        .flex()
                        .justify_center()
                        .py(rems(2.))
                        .child(Spinner::new().size(24.)),
                )
            }
            Some(Ok(state)) => {
                let state = state.clone();
                let matches = |key: &str| {
                    query.is_empty()
                        || members::match_tier(&display_name(&profiles, key), &query).is_some()
                };
                let grouped: Vec<(Group, Vec<(&String, MemberRole)>)> = Group::ALL
                    .iter()
                    .map(|group| {
                        let keys = state
                            .roles
                            .iter()
                            .filter(|(key, role)| {
                                Group::of(key, **role, &profiles, &known) == *group
                            })
                            .map(|(key, role)| (key, *role))
                            .collect();
                        (*group, keys)
                    })
                    .collect();
                let counts: Vec<(Group, usize)> = grouped
                    .iter()
                    .filter(|(_, keys)| !keys.is_empty())
                    .map(|(group, keys)| (*group, keys.len()))
                    .collect();
                // Only worth offering with more than one group, and not
                // while searching.
                if counts.len() > 1 && query.is_empty() {
                    filter = Some(self.filter_select(&counts, state.roles.len(), window, cx));
                }
                let mut shown = 0;
                for (group, keys) in grouped {
                    if self.filter.is_some_and(|only| only != group) {
                        continue;
                    }
                    let mut keys: Vec<(&String, MemberRole)> =
                        keys.into_iter().filter(|(key, _)| matches(key)).collect();
                    if keys.is_empty() && group != Group::Members {
                        continue;
                    }
                    keys.sort_by_key(|(key, _)| {
                        (**key != viewer, display_name(&profiles, key).to_lowercase())
                    });
                    shown += keys.len();
                    let mut section = div().flex().flex_col().child(group_label(
                        format!("{} · {}", group.label(), keys.len()),
                        cx,
                    ));
                    let labels = super::keys::public_key_labels(state.roles.keys());
                    for (key, role) in keys {
                        let label = labels.get(key.as_str()).cloned().unwrap_or_default();
                        section = section.child(self.member_row(
                            key,
                            role,
                            Some(&state),
                            &label,
                            window,
                            cx,
                        ));
                    }
                    list = list.child(section);
                }
                if shown == 0 {
                    list = list.child(note(
                        if query.is_empty() {
                            "No members to show."
                        } else {
                            "No members match your search."
                        },
                        cx,
                    ));
                }
                if can_add && !query.is_empty() {
                    let mut section = div()
                        .flex()
                        .flex_col()
                        .child(group_label("Not in this channel", cx));
                    if let Some(agents) = crate::agents2::Agents2::global(cx) {
                        let offers: Vec<(Team, Vec<String>)> = {
                            let agents = agents.read(cx);
                            team_offers(
                                agents.teams(),
                                &query,
                                |key| agents.find(key).is_some(),
                                |key| state.roles.contains_key(key),
                            )
                            .into_iter()
                            .map(|(team, missing)| (team.clone(), missing))
                            .collect()
                        };
                        for (team, missing) in offers {
                            section = section.child(self.team_row(&team, &missing, cx));
                        }
                    }
                    match &self.found {
                        Some((text, Ok(page))) if text.to_lowercase() == query => {
                            let others: Vec<&Person> = page
                                .people
                                .iter()
                                .filter(|p| !state.roles.contains_key(&p.pubkey))
                                // Archived identities cannot be added.
                                .filter(|p| !super::archives::hides(&self.community, &p.pubkey, cx))
                                .collect();
                            if others.is_empty() {
                                section =
                                    section.child(note("No other matching people or agents.", cx));
                            }
                            let available = others.len();
                            for person in others.into_iter().take(self.shown) {
                                section = section.child(self.candidate_row(person, cx));
                            }
                            if available > self.shown || page.more {
                                section = section.child(
                                    div().flex().justify_center().child(
                                        Button::new("members-more", "Show more results")
                                            .outline()
                                            .size(ButtonSize::Sm)
                                            .surface(Surface::Floating)
                                            .disabled(self.searching.is_some())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.show_more(available, window, cx)
                                            })),
                                    ),
                                );
                            }
                        }
                        Some((text, Err(message))) if text.to_lowercase() == query => {
                            section =
                                section.child(div().px(rems(1.)).child(error(message.clone(), cx)));
                        }
                        _ => section = section.child(note("Searching…", cx)),
                    }
                    list = list.child(section);
                }
            }
            Some(Err(_)) => {
                let members = summary.members.clone();
                let labels = super::keys::public_key_labels(members.iter());
                let mut keys: Vec<&String> = members
                    .iter()
                    .filter(|key| {
                        query.is_empty()
                            || members::match_tier(&display_name(&profiles, key), &query).is_some()
                    })
                    .collect();
                keys.sort_by_key(|key| {
                    (**key != viewer, display_name(&profiles, key).to_lowercase())
                });
                for group in [Group::Members, Group::Agents] {
                    let rows: Vec<&String> = keys
                        .iter()
                        .copied()
                        .filter(|key| {
                            Group::of(key, MemberRole::Unknown, &profiles, &known) == group
                        })
                        .collect();
                    if rows.is_empty() {
                        continue;
                    }
                    let mut section = div().flex().flex_col().child(group_label(
                        format!("{} · {}", group.label(), rows.len()),
                        cx,
                    ));
                    for key in rows {
                        let label = labels.get(key.as_str()).cloned().unwrap_or_default();
                        section = section.child(self.member_row(
                            key,
                            MemberRole::Unknown,
                            None,
                            &label,
                            window,
                            cx,
                        ));
                    }
                    list = list.child(section);
                }
            }
        }
        if self.names_failed {
            list = list.child(note(
                "Some names could not load. Public keys still identify members. Try again.",
                cx,
            ));
        }
        if self.owners_failed {
            list = list.child(note(
                "Some agent managers or their names could not load. Try again.",
                cx,
            ));
        }
        if agents_failed(cx) {
            list = list.child(
                div()
                    .px(rems(1.))
                    .child(error("Some agents could not load.", cx)),
            );
        }

        for (index, (pubkey, message)) in self.addition_errors().into_iter().enumerate() {
            let retrying = self.is_busy(&pubkey);
            let name = display_name(&profiles, &pubkey);
            let key = pubkey.clone();
            list =
                list.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(rems(0.5))
                        .px(rems(1.))
                        .text_body_sm()
                        .text_color(c.text_danger)
                        .child(format!("{name}: {message}"))
                        .child(
                            Button::new(
                                SharedString::from(format!("addition-retry-{index}")),
                                if retrying { "Retrying…" } else { "Retry" },
                            )
                            .outline()
                            .size(ButtonSize::Xs)
                            .surface(Surface::Floating)
                            .disabled(retrying || !can_add)
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.add(key.clone(), window, cx),
                            )),
                        ),
                );
        }
        if self.opening {
            list = list.child(note("Opening conversation…", cx));
        }
        if let Some(notice) = &self.notice {
            list = list.child(note(notice.clone(), cx));
        }

        Dialog::new("channel-members", "Channel members")
            .description(super::sidebar::channel_label(&summary, &profiles))
            .compact_header_gap()
            .header_action(
                IconButton::new("members-refresh", IconName::ArrowsClockwise)
                    .size(IconButtonSize::Sm)
                    .surface(Surface::Floating)
                    .tooltip("Refresh member data")
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, window, cx| this.refresh_all(window, cx))),
            )
            .on_close(cx.listener(|_, _, _, cx| cx.emit(Dismissed)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rems(0.5))
                            .child(
                                div().flex_1().min_w_0().child(
                                    SearchField::new(&self.input).surface(Surface::Floating),
                                ),
                            )
                            .children(filter),
                    )
                    .child(
                        div()
                            .id("member-list")
                            .h(px(360.))
                            .overflow_y_scroll()
                            .p(rems(0.25))
                            .rounded(px(12.))
                            .border_1()
                            .border_color(c.border_standard)
                            .child(list),
                    ),
            )
            .into_any_element()
    }
}

/// The viewer's agent list could not be read; Refresh reads it again.
fn agents_failed(cx: &App) -> bool {
    crate::agents2::Agents2::global(cx)
        .is_some_and(|agents| matches!(agents.read(cx).status(), crate::agents2::Status::Error(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::Profile;
    use core::prelude::v1::test;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn teams_match_by_name_and_offer_only_agents_here_not_in_the_channel() {
        let (ada, cy, far) = ("ab".repeat(32), "cd".repeat(32), "ef".repeat(32));
        let team = |id: &str, name: &str, members: &[&String]| Team {
            id: id.into(),
            name: name.into(),
            instructions: String::new(),
            members: members.iter().map(|key| (*key).clone()).collect(),
        };
        let teams = [
            team("crew", "Crew", &[&ada, &cy]),
            team("far", "Far", &[&far]),
        ];
        let mine = |key: &str| key == ada || key == cy;
        let offers = team_offers(&teams, "Cr", mine, |_| false);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].0.id, "crew");
        assert_eq!(offers[0].1, [ada.clone(), cy.clone()]);
        // Once all its members are in, a team is not offered again.
        assert!(team_offers(&teams, "Crew", mine, |_| true).is_empty());
        let offers = team_offers(&teams, "Crew", mine, |key| key == ada);
        assert_eq!(offers[0].1, std::slice::from_ref(&cy));
        // A team with no agents in this community is never offered.
        assert!(team_offers(&teams, "Far", mine, |_| false).is_empty());
        assert!(team_offers(&teams, "Docs", mine, |_| false).is_empty());
    }

    #[test]
    fn groups_roles_before_agents() {
        let agent = Profile {
            is_agent: true,
            ..Default::default()
        };
        let profiles: Profiles = Arc::new(HashMap::from([("a".to_string(), Arc::new(agent))]));
        assert_eq!(
            Group::of("a", MemberRole::Admin, &profiles, &HashSet::new()),
            Group::Admins
        );
        assert_eq!(
            Group::of("a", MemberRole::Member, &profiles, &HashSet::new()),
            Group::Agents
        );
        assert_eq!(
            Group::of("b", MemberRole::Bot, &profiles, &HashSet::new()),
            Group::Agents
        );
        assert_eq!(
            Group::of("b", MemberRole::Guest, &profiles, &HashSet::new()),
            Group::Members
        );
        assert_eq!(
            Group::of("b", MemberRole::Owner, &profiles, &HashSet::new()),
            Group::Owners
        );
    }
}
