//! The Membership card (`CommunityAdmin.tsx`): the community roster with
//! search, Make admin / Make member / Remove behind a confirmation, and
//! "Invite members". Every command checks the roster is current first.

use super::invite::InviteDialog;
use super::store::{self, Snapshot, Status};
use crate::community::{AppState, Community};
use crate::ui::keys::format_public_key;
use crate::ui::modal::Dismissed;
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::moderation::{Action, Change, Member, Role, allowed_actions, manages};
use buzz_relay_client::{Profiles, Session, SessionStatus};
use buzz_ui::{
    ActiveTheme, AlertDialog, Avatar, AvatarSize, Button, ButtonShape, Icon, IconButton,
    IconButtonSize, IconName, Menu, MenuItem, SearchField, Surface, TypeStyled,
};
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nostr::prelude::{PublicKey, ToBech32};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

pub(super) const STALE: &str = "The member list is out of date. Retry before making changes.";
const ACCEPTED: &str = "Change accepted by the relay.";

/// The npub of a hex key, for display only.
fn npub(pubkey: &str) -> Option<String> {
    PublicKey::from_hex(pubkey).ok()?.to_bech32().ok()
}

/// `publicKeyLabels`: distinct keys share the shortest unambiguous npub
/// suffix, starting at three characters.
pub(super) fn public_key_labels<'a>(
    pubkeys: impl IntoIterator<Item = &'a str>,
) -> HashMap<String, String> {
    let encoded: HashMap<String, String> = pubkeys
        .into_iter()
        .filter_map(|key| Some((key.to_ascii_lowercase(), npub(key)?)))
        .collect();
    let mut length = 3;
    while length < 58 {
        let suffixes: std::collections::HashSet<&str> = encoded
            .values()
            .map(|npub| &npub[npub.len() - length..])
            .collect();
        if suffixes.len() == encoded.len() {
            break;
        }
        length += 1;
    }
    encoded
        .into_iter()
        .map(|(key, npub)| {
            let label = format!("npub…{}", &npub[npub.len() - length..]);
            (key, label)
        })
        .collect()
}

/// Names that act on one identity: every target carries a key qualifier,
/// since self-declared names can look identical without being equal.
pub(super) struct Names {
    names: HashMap<String, String>,
    qualifiers: HashMap<String, String>,
}

impl Names {
    pub(super) fn new(members: &[Member], names: HashMap<String, String>) -> Self {
        Self {
            qualifiers: public_key_labels(members.iter().map(|m| m.pubkey.as_str())),
            names,
        }
    }

    pub(super) fn get(&self, pubkey: &str) -> String {
        let declared = self.names.get(pubkey).filter(|name| !name.is_empty());
        let base = declared
            .cloned()
            .or_else(|| format_public_key(pubkey))
            .unwrap_or_else(|| pubkey.to_owned());
        match (self.qualifiers.get(pubkey), declared) {
            (None, _) => base,
            (Some(qualifier), Some(_)) => format!("{base} · {qualifier}"),
            (Some(qualifier), None) => qualifier.clone(),
        }
    }
}

/// Owners first, then admins, then members; by name within each.
pub(super) fn shown<'a>(members: &'a [Member], names: &Names, query: &str) -> Vec<&'a Member> {
    let needle = query.trim().to_lowercase();
    let mut shown: Vec<(&Member, String)> = members
        .iter()
        .map(|member| (member, names.get(&member.pubkey)))
        .filter(|(member, name)| {
            needle.is_empty()
                || name.to_lowercase().contains(&needle)
                || member.pubkey.contains(&needle)
        })
        .collect();
    shown.sort_by(|(a, a_name), (b, b_name)| a.role.cmp(&b.role).then_with(|| a_name.cmp(b_name)));
    shown.into_iter().map(|(member, _)| member).collect()
}

pub struct CommunityAdmin {
    query: Entity<InputState>,
    /// The confirmation open, for the member and action it captured.
    pending: Option<(Member, Action)>,
    /// A command is running, until the roster shows its result.
    busy: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    /// The row whose actions menu is open.
    menu: Option<String>,
    invite: Option<(Entity<InviteDialog>, Subscription)>,
    /// The members whose profiles were asked for.
    wanted: Option<Arc<[Member]>>,
    /// The connection this local state belongs to; a new one starts over.
    key: Option<store::Key>,
    _demand: Subscription,
    _subscriptions: [Subscription; 2],
}

impl CommunityAdmin {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search members"));
        let subscriptions = [
            store::observe(cx, |this, cx| this.roster_changed(cx)),
            cx.observe(&query, |_, _, cx| cx.notify()),
        ];
        let mut card = Self {
            query,
            pending: None,
            busy: false,
            error: None,
            notice: None,
            menu: None,
            invite: None,
            wanted: None,
            key: store::key(cx),
            _demand: store::ensure(cx),
            _subscriptions: subscriptions,
        };
        card.roster_changed(cx);
        card
    }

    fn roster_changed(&mut self, cx: &mut Context<Self>) {
        let key = store::key(cx);
        if key != self.key {
            // `Members` is keyed by the connection: its local state resets.
            self.key = key;
            self.pending = None;
            self.busy = false;
            self.error = None;
            self.notice = None;
            self.menu = None;
            self.invite = None;
        }
        let snapshot = store::snapshot(cx);
        if let (Some(members), Some(community)) = (&snapshot.members, AppState::current(cx))
            && !self
                .wanted
                .as_ref()
                .is_some_and(|wanted| Arc::ptr_eq(wanted, members))
        {
            self.wanted = Some(members.clone());
            let people = members.iter().map(|m| m.pubkey.clone()).collect();
            community.read(cx).session.want_profiles(people);
        }
        cx.notify();
    }

    /// Sends one command, then stays busy until the roster has been
    /// re-read. `Err` only when the write itself was not confirmed.
    pub(super) fn apply(
        &mut self,
        change: Change,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let snapshot = store::snapshot(cx);
        if snapshot.error.is_some() || snapshot.refreshing {
            self.error = Some(STALE.into());
            cx.notify();
            return Task::ready(Err(STALE.into()));
        }
        let Some(community) = AppState::current(cx) else {
            return Task::ready(Err("Choose a connected community".into()));
        };
        self.busy = true;
        self.error = None;
        self.notice = None;
        cx.notify();
        let session = community.read(cx).session.clone();
        cx.spawn(async move |this, cx| {
            if let Err(error) = session.change_community_member(change).await {
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(error.clone().into());
                    this.busy = false;
                    cx.notify();
                });
                return Err(error);
            }
            let read = this.update(cx, |this, cx| {
                this.notice = Some(ACCEPTED.into());
                store::refresh(cx)
            });
            if let Ok(read) = read {
                read.await;
            }
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                cx.notify();
            });
            Ok(())
        })
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some((member, action)) = self.pending.clone() else {
            return;
        };
        let task = self.apply(action.change(&member.pubkey), cx);
        cx.spawn(async move |this, cx| {
            let _ = task.await;
            let _ = this.update(cx, |this, cx| {
                this.pending = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        store::reload(cx);
    }

    fn open_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let card = cx.entity().downgrade();
        let dialog = cx.new(|cx| InviteDialog::new(card, window, cx));
        let subscription = cx.subscribe(&dialog, |this, _, _: &Dismissed, cx| {
            this.invite = None;
            cx.notify();
        });
        self.invite = Some((dialog, subscription));
        cx.notify();
    }

    fn dismiss_menu(cx: &mut Context<Self>) -> Dismiss {
        let weak = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.menu = None;
                cx.notify();
            });
        })
    }

    fn refresh_button(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Button {
        Button::new(
            "membership-refresh",
            if snapshot.error.is_some() {
                "Retry"
            } else {
                "Refresh"
            },
        )
        .shape(ButtonShape::Control)
        .loading(snapshot.refreshing)
        .disabled(snapshot.refreshing || self.busy)
        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
    }

    /// The notice and the read error, above the list.
    fn status(&self, snapshot: &Snapshot) -> Vec<AnyElement> {
        let mut status = Vec::new();
        if let Some(notice) = &self.notice {
            status.push(
                div()
                    .text_body_sm()
                    .child(notice.clone())
                    .into_any_element(),
            );
        }
        if let Some(error) = &snapshot.error {
            let stale = if snapshot.members.is_some() {
                " The list below may be out of date."
            } else {
                ""
            };
            status.push(
                div()
                    .text_body_sm()
                    .child(format!("{error}{stale}"))
                    .into_any_element(),
            );
        }
        status
    }

    fn row(
        &self,
        member: &Member,
        names: &Names,
        snapshot: &Snapshot,
        (session, profiles): (&Session, &Profiles),
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let viewer = session.viewer();
        let own = member.pubkey == viewer;
        // A stale list must not offer another destructive command.
        let actions = if snapshot.error.is_some() {
            Vec::new()
        } else {
            allowed_actions(snapshot.role, member, own)
        };
        let label = names.get(&member.pubkey);
        let mut avatar = Avatar::new(label.clone()).size(AvatarSize::Default);
        if let Some(picture) = profiles
            .get(&member.pubkey)
            .and_then(|profile| profile.picture.clone())
        {
            avatar = avatar.image(crate::ui::media::avatar(&picture, session.media()));
        }
        let badge = match member.role {
            Role::Owner => Some(Icon::new(IconName::Crown).size(16.).color(c.text_warning)),
            Role::Admin => Some(Icon::new(IconName::Shield).size(16.).color(c.text_accent)),
            Role::Member => None,
        };
        let menu_open = self.menu.as_deref() == Some(member.pubkey.as_str());
        let menu = menu_open.then(|| {
            let mut menu = Menu::new();
            for action in &actions {
                let (action, target) = (*action, member.clone());
                let icon = match action {
                    Action::Promote => IconName::Shield,
                    Action::Demote => IconName::User,
                    Action::Remove => IconName::Minus,
                };
                menu = menu.child(
                    MenuItem::new(
                        SharedString::from(format!("membership-{}", action.verb())),
                        action.verb(),
                    )
                    .icon(icon)
                    .icon_size(14.)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        this.pending = Some((target.clone(), action));
                        cx.notify();
                    })),
                );
            }
            overlay::dropdown(window, Align::End, menu, Self::dismiss_menu(cx))
        });
        let key = member.pubkey.clone();
        div()
            .id(SharedString::from(format!("member-{}", member.pubkey)))
            .flex()
            .items_center()
            .gap(rems(0.75))
            .py(rems(0.5))
            .child(avatar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .min_w_0()
                            .gap(rems(0.375))
                            .text_body()
                            .child(div().min_w_0().truncate().child(label.clone()))
                            .children(badge.map(|badge| div().flex_none().child(badge))),
                    )
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child(format!(
                                "{}{}",
                                member.role.label(),
                                if own { " · You" } else { "" }
                            )),
                    ),
            )
            .when(!actions.is_empty(), |this| {
                this.child(
                    div()
                        .relative()
                        .flex_none()
                        .child(
                            IconButton::new(
                                SharedString::from(format!("member-actions-{}", member.pubkey)),
                                IconName::DotsThree,
                            )
                            .size(IconButtonSize::Sm)
                            .disabled(self.busy || snapshot.refreshing)
                            .when(!menu_open, |b| b.tooltip(format!("Actions for {label}")))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.menu = Some(key.clone());
                                    cx.notify();
                                },
                            )),
                        )
                        .children(menu),
                )
            })
            .into_any_element()
    }

    fn confirmation(
        &self,
        member: &Member,
        action: Action,
        names: &Names,
        snapshot: &Snapshot,
        viewer: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stale = snapshot.error.is_some();
        let locked = stale || snapshot.refreshing;
        // Revalidate the captured intent against the current roster.
        let confirmable = !locked
            && snapshot
                .members
                .as_ref()
                .and_then(|members| members.iter().find(|m| m.pubkey == member.pubkey))
                .is_some_and(|target| {
                    allowed_actions(snapshot.role, target, target.pubkey == viewer)
                        .contains(&action)
                });
        let confirm = Button::new("membership-confirm", action.verb())
            .loading(self.busy)
            .disabled(self.busy || !confirmable)
            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx)));
        AlertDialog::new(format!("{}: {}?", action.verb(), names.get(&member.pubkey)))
            .description(if stale { STALE } else { action.description() })
            .action(
                Button::new("membership-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pending = None;
                        cx.notify();
                    })),
            )
            .action(if action == Action::Remove {
                confirm.destructive()
            } else {
                confirm.prominent()
            })
            .into_any_element()
    }

    fn members(
        &mut self,
        community: &Entity<Community>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let c = *cx.colors();
        let snapshot = store::snapshot(cx);
        if matches!(snapshot.status, Status::Idle | Status::Loading) {
            return vec![
                div()
                    .text_body()
                    .child("Loading members…")
                    .into_any_element(),
            ];
        }
        let members = match &snapshot.members {
            Some(members) if manages(snapshot.role) => members.clone(),
            _ => {
                let message = if snapshot.members.is_none() {
                    "This community does not publish a member list, so it has no member \
                     administration."
                } else {
                    "Only community owners and admins can invite people or manage members."
                };
                return vec![
                    div()
                        .mt(rems(1.))
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(rems(0.75))
                        .children(self.status(&snapshot))
                        .when(snapshot.error.is_none(), |this| {
                            this.child(div().text_body().child(message))
                        })
                        .child(self.refresh_button(&snapshot, cx))
                        .into_any_element(),
                ];
            }
        };
        let stale = snapshot.error.is_some();
        let (session, profiles) = {
            let community = community.read(cx);
            (
                community.session.clone(),
                community.snapshot.profiles.clone(),
            )
        };
        let viewer = session.viewer().to_owned();
        let names = Names::new(
            &members,
            members
                .iter()
                .filter_map(|member| {
                    let profile = profiles.get(&member.pubkey)?;
                    (!profile.name_is_fallback)
                        .then(|| (member.pubkey.clone(), profile.name.clone()))
                })
                .collect(),
        );
        let query = self.query.read(cx).value().to_string();
        let rows: Vec<AnyElement> = shown(&members, &names, &query)
            .into_iter()
            .map(|member| self.row(member, &names, &snapshot, (&session, &profiles), window, cx))
            .collect();
        let empty = rows.is_empty();
        let mut parts = vec![
            div()
                .mt(rems(1.))
                .flex()
                .justify_end()
                .gap(rems(0.5))
                .child(self.refresh_button(&snapshot, cx))
                // A stale roster cannot prove the viewer still manages this
                // community.
                .when(!stale, |this| {
                    this.child(
                        Button::new("membership-invite", "Invite members")
                            .shape(ButtonShape::Control)
                            .prominent()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_invite(window, cx)),
                            ),
                    )
                })
                .into_any_element(),
        ];
        parts.extend(self.status(&snapshot));
        if let Some(error) = &self.error {
            parts.push(div().text_body_sm().child(error.clone()).into_any_element());
        }
        parts.push(
            div()
                .mt(rems(1.5))
                .mb(rems(0.5))
                .flex()
                .gap(rems(0.25))
                .text_label()
                .child("Members")
                .child(
                    div()
                        .text_color(c.text_subtle)
                        .child(members.len().to_string()),
                )
                .into_any_element(),
        );
        parts.push(
            div()
                .p(rems(1.))
                .rounded(rems(0.75))
                .border_1()
                .border_color(c.border_standard)
                .child(SearchField::new(&self.query))
                .child(div().mt(rems(0.75)).flex().flex_col().children(rows))
                .when(empty, |this| {
                    this.child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child("No members match."),
                    )
                })
                .into_any_element(),
        );
        if let Some((member, action)) = self.pending.clone() {
            let dismiss: Option<Dismiss> = (!self.busy).then(|| {
                let weak = cx.entity().downgrade();
                Rc::new(move |_: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        this.pending = None;
                        cx.notify();
                    });
                }) as Dismiss
            });
            let dialog = self.confirmation(&member, action, &names, &snapshot, &viewer, cx);
            parts.push(overlay::modal(window, dialog, dismiss).into_any_element());
        }
        if let Some((dialog, _)) = &self.invite {
            let adding = dialog.read(cx).adding();
            let dismiss: Option<Dismiss> = (!adding).then(|| {
                let weak = cx.entity().downgrade();
                Rc::new(move |_: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        this.invite = None;
                        cx.notify();
                    });
                }) as Dismiss
            });
            parts.push(overlay::modal(window, dialog.clone(), dismiss).into_any_element());
        }
        parts
    }
}

impl Render for CommunityAdmin {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let community = AppState::current(cx);
        let name = community
            .as_ref()
            .map(|community| community.read(cx).name());
        let connected = community
            .as_ref()
            .filter(|community| community.read(cx).snapshot.status == SessionStatus::Ready);
        let body = match connected.cloned() {
            Some(community) => self.members(&community, window, cx),
            None => vec![
                div()
                    .text_body()
                    .child("Choose a connected community to manage members.")
                    .into_any_element(),
            ],
        };
        div()
            .id("community-admin")
            .flex()
            .flex_col()
            .gap(rems(0.25))
            .text_color(c.text_standard)
            .child(div().mb(rems(0.25)).text_label().child("Membership"))
            .child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(match name {
                        Some(name) => format!("Manage members and access to {name}."),
                        None => "Manage members and community access.".to_owned(),
                    }),
            )
            .children(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn member(c: char, role: Role) -> Member {
        Member {
            pubkey: c.to_string().repeat(64),
            role,
        }
    }

    #[test]
    fn qualifies_namesakes_with_distinct_key_suffixes() {
        let (a, b) = ('a'.to_string().repeat(64), 'b'.to_string().repeat(64));
        let members = [member('a', Role::Member), member('b', Role::Member)];
        let names = Names::new(
            &members,
            HashMap::from([(a.clone(), "Bob".into()), (b.clone(), "Bob\u{200B}".into())]),
        );
        let labels = public_key_labels([a.as_str(), b.as_str()]);
        assert_ne!(labels[&a], labels[&b]);
        assert_eq!(names.get(&a), format!("Bob · {}", labels[&a]));
        assert_eq!(names.get(&b), format!("Bob\u{200B} · {}", labels[&b]));
        // Without a declared name, the qualifier alone.
        let bare = Names::new(&members, HashMap::new());
        assert_eq!(bare.get(&a), labels[&a]);
        assert!(labels[&a].starts_with("npub…"));
    }

    #[test]
    fn suffixes_grow_until_unambiguous() {
        let keys: Vec<String> = (0..200u8)
            .map(|i| format!("{:064x}", u128::from(i) * 7919))
            .collect();
        let labels = public_key_labels(keys.iter().map(String::as_str));
        let distinct: std::collections::HashSet<_> = labels.values().collect();
        assert_eq!(distinct.len(), keys.len());
        assert!(
            labels
                .values()
                .all(|label| label.len() >= "npub…".len() + 3)
        );
    }

    #[test]
    fn lists_owners_admins_then_members_and_searches_names_and_keys() {
        let members = [
            member('2', Role::Member),
            member('1', Role::Admin),
            member('0', Role::Owner),
            member('3', Role::Member),
        ];
        let names = Names::new(
            &members,
            HashMap::from([
                ("2".repeat(64), "Zed".into()),
                ("3".repeat(64), "Amy".into()),
                ("1".repeat(64), "Ada".into()),
            ]),
        );
        let order: Vec<char> = shown(&members, &names, "")
            .iter()
            .map(|m| m.pubkey.chars().next().unwrap())
            .collect();
        assert_eq!(order, ['0', '1', '3', '2']);
        let found = shown(&members, &names, "  ZE ");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pubkey, "2".repeat(64));
        assert_eq!(shown(&members, &names, &"3".repeat(10)).len(), 1);
        assert!(shown(&members, &names, "nobody").is_empty());
    }
}
