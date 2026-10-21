//! The profile panel (`bundled/profiles/ProfilePanel.tsx`): a person or
//! agent beside the conversation, opened from a mention, an avatar or the
//! members list, or as the viewer's own profile from the account menu.
//! Tabs: Info, Channels and, for an agent, Activity.
//!
//! It is a channel panel for profile links (`nostr:npub…` and the app's
//! `buzz:agent-profile:<hex>`), so a profile opens as a tab in the
//! channel's tab pane; the companion dock shows the same view.

mod agent;

use super::channel_panels::{self, ChannelPanel, Opening};
use super::keys::npub;
use super::media;
use super::message::channel_icon;
use crate::agents2::AgentConversation;
use crate::community::Community;
use buzz_relay_client::{ChannelType, PresenceState, RosterStatus, StatusWatch};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, ButtonSize, ButtonVariant, Icon, IconName,
    NavigationItem, Presence, Tab, TabVariant, Tabs, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nostr::nips::nip19::{FromBech32, ToBech32};
use nostr::prelude::PublicKey;
use std::rc::Rc;
use std::time::Duration;

/// The panel's key among channel panels.
pub const KEY: &str = "buzz.profiles/profile";
/// How long "Loading profile…" waits for the profile before saying there
/// is none in this community.
const PROFILE_WAIT: Duration = Duration::from_secs(10);

/// A person's profile link (`profileTarget`): `nostr:npub…`, or with
/// the agent hint the app-local `buzz:agent-profile:<hex>`.
pub fn target(pubkey: &str, agent: bool) -> Option<String> {
    let key = PublicKey::from_hex(pubkey).ok()?;
    Some(match agent {
        true => format!("buzz:agent-profile:{}", key.to_hex()),
        false => format!("nostr:{}", key.to_bech32().ok()?),
    })
}

/// The app-local link that opens an agent's profile on its activity
/// (`profileActivityViewTarget`); never a public identity claim.
pub fn activity_target(pubkey: &str) -> Option<String> {
    let key = PublicKey::from_hex(pubkey).ok()?;
    Some(format!("buzz:profile?agent={}&tab=activity", key.to_hex()))
}

/// The agent an [`activity_target`] names (`profileActivityViewKey`).
pub fn parse_activity_target(target: &str) -> Option<String> {
    let hex = target
        .strip_prefix("buzz:profile?agent=")?
        .strip_suffix("&tab=activity")?;
    (activity_target(hex).as_deref() == Some(target)).then(|| hex.to_string())
}

/// The key a profile link names and whether it carries the agent hint
/// (`profilePanelKey`, `profileAgentHint`). An activity link is an
/// agent's.
pub fn parse_target(target: &str) -> Option<(String, bool)> {
    if let Some(agent) = parse_activity_target(target) {
        return Some((agent, true));
    }
    if let Some(hex) = target.strip_prefix("buzz:agent-profile:") {
        let valid = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        return valid.then(|| (hex.to_string(), true));
    }
    let npub = target.strip_prefix("nostr:")?;
    if !npub.starts_with("npub1") {
        return None;
    }
    Some((PublicKey::from_bech32(npub).ok()?.to_hex(), false))
}

/// Registers the panel for profile links.
pub fn init(cx: &mut App) {
    channel_panels::register(
        ChannelPanel {
            key: KEY,
            order: 0,
            title: "Profile",
            icon: IconName::User,
            matches: channel_panels::matches(|target| parse_target(target).is_some()),
            app_launcher: None,
            launcher: None,
            menu: None,
            build: channel_panels::build(|opening, window, cx| {
                cx.new(|cx| ProfilePanel::from_opening(opening, window, cx))
                    .into()
            }),
        },
        cx,
    );
    channel_panels::register_title(
        KEY,
        |view, cx| {
            let panel = view.clone().downcast::<ProfilePanel>().ok()?;
            Some(panel.read(cx).name(cx))
        },
        cx,
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Info,
    Channels,
    /// The agent's activity, embedded (`PanelView` of Agent Activity).
    Activity,
}

pub struct ProfilePanel {
    community: Entity<Community>,
    pubkey: String,
    /// The link said this is an agent; presentation only.
    agent_hint: bool,
    /// The conversation the panel was opened from.
    context: Option<String>,
    /// The thread root of the message the panel was opened from.
    root: Option<String>,
    close: Option<channel_panels::Close>,
    tab: Section,
    /// Opened for activity and no tab picked since: an Agents agent shows
    /// Info, where its type's sessions are.
    activity_requested: bool,
    /// The embedded Agent Activity panel, once shown.
    activity: Option<AnyView>,
    /// The profile read has had its chance to answer.
    settled: bool,
    copy_status: Option<SharedString>,
    /// The copy field that just copied, by label.
    copied: Option<&'static str>,
    opening_message: bool,
    message_error: Option<SharedString>,
    focus: FocusHandle,
    _status: StatusWatch,
    _wait: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ProfilePanel {
    fn from_opening(opening: Opening, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (pubkey, agent_hint) = parse_target(&opening.target).unwrap_or_default();
        let mut panel = Self::new(opening.community, pubkey, agent_hint, window, cx);
        panel.context = Some(opening.channel);
        panel.root = opening.root;
        panel.close = Some(opening.close);
        if parse_activity_target(&opening.target).is_some() {
            panel.tab = Section::Activity;
            panel.activity_requested = true;
        }
        panel
    }

    pub fn new(
        community: Entity<Community>,
        pubkey: String,
        agent_hint: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let status = community
            .read(cx)
            .session
            .watch_statuses(std::slice::from_ref(&pubkey));
        community.update(cx, |community, _| community.hold_profile_presence(&pubkey));
        let observe = cx.observe(&community, |this, _, cx| {
            if !this.settled && this.profile(cx).is_some() {
                this.settled = true;
            }
            cx.notify();
        });
        let mut subscriptions = vec![observe];
        // An Agents2 agent's sections follow its saved settings.
        subscriptions.extend(
            crate::agents2::Agents2::global(cx)
                .map(|agents| cx.observe(&agents, |_, _, cx| cx.notify())),
        );
        subscriptions.push(cx.on_release(|this: &mut Self, cx| {
            let pubkey = this.pubkey.clone();
            this.community.update(cx, |community, _| {
                community.release_profile_presence(&pubkey)
            });
        }));
        let mut panel = Self {
            community,
            pubkey,
            agent_hint,
            context: None,
            root: None,
            close: None,
            tab: Section::Info,
            activity_requested: false,
            activity: None,
            settled: false,
            copy_status: None,
            copied: None,
            opening_message: false,
            message_error: None,
            focus: cx.focus_handle(),
            _status: status,
            _wait: None,
            _subscriptions: subscriptions,
        };
        panel.load(cx);
        panel
    }

    pub fn pubkey(&self) -> &str {
        &self.pubkey
    }

    fn session(&self, cx: &App) -> std::sync::Arc<buzz_relay_client::Session> {
        self.community.read(cx).session.clone()
    }

    fn profile(&self, cx: &App) -> Option<std::sync::Arc<buzz_relay_client::Profile>> {
        self.community
            .read(cx)
            .snapshot
            .profiles
            .get(&self.pubkey)
            .cloned()
    }

    /// Asks the session for the profile, and gives it a while to answer.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.settled = self.profile(cx).is_some();
        self.session(cx).want_profiles(vec![self.pubkey.clone()]);
        self._wait = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PROFILE_WAIT).await;
            let _ = this.update(cx, |this, cx| {
                this.settled = true;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// A known agent: its profile says so, or it runs on one of the
    /// viewer's hosts (`useKnownAgentPubkeys`).
    fn known_agent(&self, cx: &App) -> bool {
        self.profile(cx).is_some_and(|p| p.is_agent)
            || crate::ui::typing::owned_agents(cx).contains(&self.pubkey)
    }

    /// The agent's owner, when its attestation verifies.
    fn verified_owner(&self, cx: &App) -> Option<String> {
        self.profile(cx)?.attested_owner.clone()
    }

    /// The name, or "Unknown agent" or the short key without a profile.
    pub fn name(&self, cx: &App) -> SharedString {
        match self.profile(cx) {
            Some(profile) => profile.name.clone().into(),
            None if self.known_agent(cx) || self.agent_hint => "Unknown agent".into(),
            None => short_key(&self.pubkey).into(),
        }
    }

    /// The tabs shown, and the one selected: a tab that went away falls
    /// back to Info. Opened for activity, an Agents agent shows Info,
    /// where its type's sessions are, until the reader picks a tab.
    fn sections(&self, cx: &App) -> (Vec<Section>, Section) {
        let mut sections = vec![Section::Info, Section::Channels];
        if self.known_agent(cx) || self.agent_hint {
            sections.push(Section::Activity);
        }
        let plugin = crate::agents2::Agents2::global(cx)
            .is_some_and(|agents| agents.read(cx).find(&self.pubkey).is_some());
        let selected = match sections.contains(&self.tab) {
            _ if self.activity_requested && self.tab == Section::Activity && plugin => {
                Section::Info
            }
            true => self.tab,
            false => Section::Info,
        };
        (sections, selected)
    }

    fn select(&mut self, section: Section, cx: &mut Context<Self>) {
        self.activity_requested = false;
        self.tab = section;
        cx.notify();
    }

    /// The agent's activity in the conversation the panel opened from.
    fn activity(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let view = match &self.activity {
            Some(view) => view.clone(),
            None => {
                let close = self.close.clone().unwrap_or_else(|| Rc::new(|_, _| {}));
                let opening = channel_panels::Opening {
                    community: self.community.clone(),
                    channel: self.context.clone().unwrap_or_default(),
                    target: crate::agent_activity::target(
                        &self.pubkey,
                        self.context.as_deref(),
                        None,
                    ),
                    close,
                    thread: None,
                    root: self.root.clone(),
                };
                let view: AnyView = cx
                    .new(|cx| {
                        crate::ui::agent_activity::panel::ActivityPanel::new(opening, window, cx)
                    })
                    .into();
                self.activity = Some(view.clone());
                view
            }
        };
        vec![div().h(rems(36.)).min_w_0().child(view).into_any_element()]
    }

    // ----- actions -----

    /// Whether Message is offered: someone else, and for an agent only
    /// one the viewer manages here (`messageable`).
    fn can_message(&self, cx: &App) -> bool {
        let viewer = self.community.read(cx).session.viewer().to_string();
        let agent = self.known_agent(cx) || self.agent_hint;
        viewer != self.pubkey
            && (!agent || crate::ui::typing::owned_agents(cx).contains(&self.pubkey))
    }

    fn open_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.opening_message || !self.can_message(cx) {
            return;
        }
        self.opening_message = true;
        self.message_error = None;
        let session = self.session(cx);
        let pubkey = self.pubkey.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.open_dm(vec![pubkey]).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.opening_message = false;
                match result {
                    Ok(id) => crate::ui::navigation::open_conversation(id, window, cx),
                    Err(error) => this.message_error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn copy_key(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(npub(&self.pubkey)));
        self.copy_status = Some("Public key copied.".into());
        cx.notify();
    }

    fn copy_value(&mut self, label: &'static str, value: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.copied = Some(label);
        super::notice::success(format!("Copied {}", label.to_lowercase()), cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.copied == Some(label) {
                    this.copied = None;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ----- rendering -----

    fn identity(&self, cx: &App) -> AnyElement {
        let snapshot = self.community.read(cx).snapshot.clone();
        let media = self.community.read(cx).session.media().clone();
        let name = self.name(cx);
        let presence = snapshot.presence.get(&self.pubkey).copied();
        let mut avatar = Avatar::new(name.clone())
            .size(AvatarSize::Px(80.))
            .presence(presence.map(|state| match state {
                PresenceState::Online => Presence::Online,
                PresenceState::Away => Presence::Away,
                PresenceState::Offline => Presence::Offline,
            }));
        if self.known_agent(cx) || self.agent_hint {
            avatar = avatar.squircle();
        }
        if let Some(picture) = self.profile(cx).and_then(|p| p.picture.clone()) {
            avatar = avatar.image(media::avatar(&picture, &media));
        }
        div()
            .flex()
            .items_center()
            .gap(space::S4)
            .min_w_0()
            .child(div().flex_none().child(avatar))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(div().text_heading().child(name))
                    .children(presence.map(|state| {
                        div()
                            .text_body_sm()
                            .text_color(cx.colors().text_subtle)
                            .child(presence_label(state))
                    })),
            )
            .into_any_element()
    }

    fn info(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = *cx.colors();
        let profile = self.profile(cx);
        let snapshot = self.community.read(cx).snapshot.clone();
        let mut items: Vec<AnyElement> = Vec::new();
        items.extend(crate::ui::status::display(
            snapshot.statuses.get(&self.pubkey),
            &self.community,
            cx,
        ));
        if self.can_message(cx) {
            items.push(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(space::S2)
                    .child(
                        Button::new("profile-message", "Message")
                            .size(ButtonSize::Sm)
                            .loading(self.opening_message)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_message(window, cx)),
                            ),
                    )
                    .children(
                        self.message_error
                            .clone()
                            .map(|error| div().text_color(c.text_danger).child(error)),
                    )
                    .into_any_element(),
            );
        }
        if let Some(about) = profile
            .as_ref()
            .and_then(|p| p.about.clone())
            .filter(|about| !about.is_empty())
        {
            items.push(div().child(about).into_any_element());
        }
        // An Agents2 agent's sections and Delete, then a known agent's
        // latest activity (`ProfileActivity`).
        let conversation = self.context.clone().map(|channel_id| AgentConversation {
            channel_id,
            root_id: self.root.clone(),
        });
        items.extend(crate::ui::agents2::profile_sections(
            &self.pubkey,
            conversation,
            window,
            cx,
        ));
        if self.known_agent(cx) {
            let this = cx.entity().downgrade();
            items.extend(crate::ui::agent_activity::profile::latest_activity(
                &self.community,
                &self.pubkey,
                self.context.as_deref(),
                Some(Rc::new(move |_, cx| {
                    let _ = this.update(cx, |this, cx| this.select(Section::Activity, cx));
                })),
                cx,
            ));
        }
        items.push(
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap(space::S2)
                        .child(
                            div()
                                .text_body()
                                .text_color(c.text_subtle)
                                .child("Public key"),
                        )
                        .child(
                            Button::new("profile-copy-npub", "Copy")
                                .size(ButtonSize::Sm)
                                .variant(ButtonVariant::Ghost)
                                .icon(IconName::Copy)
                                .on_click(cx.listener(|this, _, _, cx| this.copy_key(cx))),
                        ),
                )
                .child(
                    div()
                        .p(space::S3)
                        .rounded(radius::CONTROL)
                        .bg(c.surface_inset)
                        .text_mono()
                        .child(npub(&self.pubkey)),
                )
                .children(
                    self.copy_status
                        .clone()
                        .map(|status| div().text_color(c.text_subtle).child(status)),
                )
                .into_any_element(),
        );
        items.extend(self.agent_identity_items(cx));
        if let Some(nip05) = profile.as_ref().and_then(|p| p.nip05.clone()) {
            items.push(self.copy_field("NIP-05", nip05, cx));
        }
        if profile.is_none() {
            let (note, retry) = match self.settled {
                false => ("Loading profile…", false),
                true => ("No profile metadata is available in this community.", true),
            };
            items.push(div().child(note).into_any_element());
            if retry {
                items.push(
                    div()
                        .child(
                            Button::new("profile-retry", "Retry profile")
                                .size(ButtonSize::Sm)
                                .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                        )
                        .into_any_element(),
                );
            }
        }
        items
    }

    /// A field that copies its value (`ProfileCopyField`).
    fn copy_field(&self, label: &'static str, value: String, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let copied = self.copied == Some(label);
        let copy = value.clone();
        div()
            .id(SharedString::from(format!("profile-copy-{label}")))
            .group("profile-copy-field")
            .flex()
            .items_center()
            .gap(space::S3)
            .p(space::S4)
            .border_1()
            .border_color(c.border_standard)
            .rounded(radius::CONTROL)
            .bg(c.surface_panel)
            .cursor_pointer()
            .hover(|this| this.bg(c.surface_inset))
            .tooltip(buzz_ui::Tooltip::text(format!("Copy {label}")))
            .on_click(cx.listener(move |this, _, _, cx| this.copy_value(label, copy.clone(), cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap(space::S2)
                    .min_w_0()
                    .child(div().text_body().child(label))
                    .child(div().truncate().text_color(c.text_subtle).child(value)),
            )
            .child(
                div()
                    .flex_none()
                    .invisible()
                    .when(copied, |this| this.visible())
                    .group_hover("profile-copy-field", |this| this.visible())
                    .child(
                        Icon::new(if copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .size(16.),
                    ),
            )
            .into_any_element()
    }

    fn channels(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = *cx.colors();
        let snapshot = self.community.read(cx).snapshot.clone();
        let note = |text: &'static str| div().p(space::S4).child(text).into_any_element();
        let listed = matches!(
            snapshot.roster,
            RosterStatus::Ready | RosterStatus::Error(_)
        );
        let visible = |channel: &&buzz_relay_client::ChannelSummary| {
            !channel.archived && !channel.hidden && channel.members.contains(&self.pubkey)
        };
        let channels: Vec<_> = snapshot
            .channels
            .iter()
            .filter(visible)
            .filter(|channel| {
                matches!(
                    channel.channel_type,
                    Some(ChannelType::Stream | ChannelType::Forum)
                )
            })
            .cloned()
            .collect();
        let unclassified = listed
            && snapshot
                .channels
                .iter()
                .filter(visible)
                .any(|channel| channel.channel_type.is_none());
        let mut items = Vec::new();
        match &snapshot.roster {
            RosterStatus::Loading => items.push(note("Loading channels…")),
            RosterStatus::Error(_) => items.push(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(space::S2)
                    .p(space::S4)
                    .child("Could not finish loading channels.")
                    .child(
                        Button::new("profile-retry-channels", "Retry channels")
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.session(cx).visible())),
                    )
                    .into_any_element(),
            ),
            RosterStatus::Ready => {}
        }
        if unclassified {
            items.push(
                div()
                    .p(space::S4)
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child("Some memberships for this identity are unclassified and omitted.")
                    .into_any_element(),
            );
        }
        if snapshot.roster == RosterStatus::Ready && channels.is_empty() {
            items.push(note(if unclassified {
                "No matching channels with a known visible type; other memberships could not \
                 be classified."
            } else {
                "No visible channels with verified membership for this identity."
            }));
        }
        if !channels.is_empty() {
            items.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S1)
                    .children(channels.iter().enumerate().map(|(index, channel)| {
                        let id = channel.id.clone();
                        NavigationItem::new(
                            SharedString::from(format!("profile-channel-{index}")),
                            channel.name.clone(),
                        )
                        .icon(channel_icon(channel))
                        .on_click(move |_, window, cx| {
                            crate::ui::navigation::open_conversation(id.clone(), window, cx)
                        })
                    }))
                    .into_any_element(),
            );
        }
        items
    }
}

impl Focusable for ProfilePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ProfilePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let (sections, selected) = self.sections(cx);
        let mut tabs = Tabs::new(TabVariant::Panel);
        for section in sections {
            let (id, label) = match section {
                Section::Info => ("profile-tab-info", "Info"),
                Section::Channels => ("profile-tab-channels", "Channels"),
                Section::Activity => ("profile-tab-activity", "Activity"),
            };
            tabs = tabs.tab(
                Tab::new(id, label)
                    .selected(section == selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(section, cx))),
            );
        }
        let content = match selected {
            Section::Info => self.info(window, cx),
            Section::Channels => self.channels(cx),
            Section::Activity => self.activity(window, cx),
        };
        let close = self.close.clone();
        div()
            .id("profile-panel")
            .key_context("ProfilePanel")
            .track_focus(&self.focus)
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && let Some(close) = &close
                {
                    cx.stop_propagation();
                    close(window, cx);
                }
            })
            .flex()
            .flex_col()
            .gap(space::S6)
            .p(space::S5)
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .bg(c.surface_panel)
            .text_body()
            .text_color(c.text_standard)
            .child(self.identity(cx))
            .child(
                div().flex().flex_col().child(tabs).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(space::S6)
                        .pt(space::S4)
                        .min_w_0()
                        .children(content),
                ),
            )
    }
}

/// "Active", "Away" or "Offline".
pub fn presence_label(state: PresenceState) -> &'static str {
    match state {
        PresenceState::Online => "Active",
        PresenceState::Away => "Away",
        PresenceState::Offline => "Offline",
    }
}

/// `formatPublicKey`: `npub1abcd…wxyz`.
fn short_key(pubkey: &str) -> String {
    let npub = npub(pubkey);
    match npub.starts_with("npub1") && npub.len() > 16 {
        true => format!("{}…{}", &npub[..10], &npub[npub.len() - 4..]),
        false => npub,
    }
}

/// Opens `pubkey`'s profile as its own companion panel.
pub fn view(
    community: Entity<Community>,
    pubkey: String,
    close: channel_panels::Close,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ProfilePanel> {
    cx.new(|cx| {
        let mut panel = ProfilePanel::new(community, pubkey, false, window, cx);
        panel.close = Some(close);
        panel
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn profile_targets_round_trip() {
        let key = buzz_identity::Identity::ephemeral().public_key().to_hex();
        let person = target(&key, false).unwrap();
        assert!(person.starts_with("nostr:npub1"));
        assert_eq!(parse_target(&person), Some((key.clone(), false)));
        let agent = target(&key, true).unwrap();
        assert_eq!(agent, format!("buzz:agent-profile:{key}"));
        assert_eq!(parse_target(&agent), Some((key.clone(), true)));
        assert_eq!(parse_target("nostr:nsec1abc"), None);
        assert_eq!(parse_target("buzz:agent-profile:XYZ"), None);
        assert_eq!(parse_target("https://example.com"), None);
        assert_eq!(target("nothex", false), None);
        let activity = activity_target(&key).unwrap();
        assert_eq!(activity, format!("buzz:profile?agent={key}&tab=activity"));
        assert_eq!(parse_target(&activity), Some((key.clone(), true)));
        assert_eq!(parse_activity_target(&activity), Some(key.clone()));
        assert_eq!(
            parse_activity_target(&format!("buzz:profile?agent={key}&tab=info")),
            None
        );
    }

    #[test]
    fn labels_follow_buzz_app() {
        assert_eq!(presence_label(PresenceState::Online), "Active");
    }

    #[test]
    fn short_keys_keep_both_ends() {
        let key = buzz_identity::Identity::ephemeral().public_key().to_hex();
        let short = short_key(&key);
        assert!(short.starts_with("npub1") && short.contains('…'));
        assert_eq!(short.chars().count(), 15);
    }
}
