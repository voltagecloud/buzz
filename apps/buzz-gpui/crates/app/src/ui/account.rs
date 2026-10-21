//! The header's profile button and account menu (ui.md §2.5): who you
//! are, your availability and status, and Settings. Signing out is in
//! Settings → Profile, as in buzz-app.

use super::media;
use super::overlay::{self, Align, Dismiss};
use super::settings::OpenSettings;
use super::status::{Closed, StatusEditor};
use crate::community::{AppState, IdentityState};
use buzz_relay_client::{PresenceMode, PresenceState, UserStatus};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, ButtonVariant, Icon, IconButton, IconButtonShape,
    IconButtonSize, IconName, MaterialStyled, Menu, MenuItem, MenuSeparator, Presence, Spinner,
    TypeStyled, tokens::radius,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// Opens the viewer's profile.
pub type OnProfile = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Menu,
    Availability,
}

enum Modal {
    Status {
        editor: Entity<StatusEditor>,
        _subscription: Subscription,
    },
    /// A plugin's account action.
    /// A plugin account action's dialog, by the action's key.
    Plugin(&'static str, AnyView),
}

pub struct Account {
    /// Whether Settings is the current page, for the menu's check mark.
    pub settings_open: bool,
    /// Opens the viewer's profile in the companion panel, set by the
    /// workspace while a community is selected.
    pub on_profile: Option<OnProfile>,
    open: Option<Open>,
    modal: Option<Modal>,
    /// The viewer's status in the selected community, by community.
    status: Option<(EntityId, Option<UserStatus>)>,
    status_loading: bool,
    status_error: Option<SharedString>,
    /// An availability choice waiting to be observed on the relay.
    availability: Option<Change>,
    changes: usize,
    /// Communities whose own profile was read again once live.
    refreshed: Vec<EntityId>,
}

/// Confirmation feedback for an availability choice: one 15 s deadline
/// per click, never a retry.
#[derive(Clone, Copy)]
struct Change {
    target: PresenceState,
    generation: usize,
    failed: bool,
}

const CONFIRM_AVAILABILITY: std::time::Duration = std::time::Duration::from_secs(15);

impl Account {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            settings_open: false,
            on_profile: None,
            open: None,
            modal: None,
            status: None,
            status_loading: false,
            status_error: None,
            availability: None,
            changes: 0,
            refreshed: Vec::new(),
        }
    }

    /// Once the roster is verified and live, reads the viewer's profile
    /// again, so the avatar is not a stale copy (`ProfileButton.tsx`).
    fn refresh_profile(&mut self, cx: &App) {
        let Some(community) = AppState::current(cx) else {
            return;
        };
        let id = community.entity_id();
        let community = community.read(cx);
        let snapshot = &community.snapshot;
        if self.refreshed.contains(&id)
            || snapshot.roster != buzz_relay_client::RosterStatus::Ready
            || !snapshot.live
        {
            return;
        }
        self.refreshed.push(id);
        community.session.refresh_own_profile();
    }

    fn confirm_availability(&mut self, mode: PresenceMode, cx: &mut Context<Self>) {
        self.changes += 1;
        let generation = self.changes;
        self.availability = Some(Change {
            target: match mode {
                PresenceMode::Auto => PresenceState::Online,
                PresenceMode::Away => PresenceState::Away,
                PresenceMode::Offline => PresenceState::Offline,
            },
            generation,
            failed: false,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CONFIRM_AVAILABILITY).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(change) = &mut this.availability
                    && change.generation == generation
                {
                    change.failed = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_open(&mut self, open: Option<Open>, cx: &mut Context<Self>) {
        let opening = open == Some(Open::Menu) && self.open.is_none();
        self.open = open;
        if opening {
            self.refresh_status(cx);
        }
        cx.notify();
    }

    fn dismiss(cx: &mut Context<Self>, to: Option<Open>) -> Dismiss {
        let account = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = account.update(cx, |this, cx| this.set_open(to, cx));
        })
    }

    /// Reads the viewer's current status fresh, as the menu opens.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let Some(community) = AppState::current(cx) else {
            return;
        };
        let id = community.entity_id();
        let session = community.read(cx).session.clone();
        self.status_loading = true;
        self.status_error = None;
        cx.spawn(async move |this, cx| {
            let result = session.own_status().await;
            let _ = this.update(cx, |this, cx| {
                this.status_loading = false;
                match result {
                    Ok(status) => this.status = Some((id, status)),
                    Err(error) => {
                        this.status_error =
                            Some(format!("Couldn’t load your status. {error}").into())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn current_status(&self, cx: &App) -> Option<UserStatus> {
        let community = AppState::current(cx)?;
        match &self.status {
            Some((id, status)) if *id == community.entity_id() => status.clone(),
            _ => None,
        }
    }

    fn edit_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = AppState::current(cx) else {
            return;
        };
        let id = community.entity_id();
        let current = self.current_status(cx);
        let editor = cx.new(|cx| StatusEditor::new(community, current, window, cx));
        let _subscription = cx.subscribe(&editor, move |this, _, closed: &Closed, cx| {
            if let Closed(Some(saved)) = closed {
                this.status = Some((id, saved.clone()));
            }
            this.modal = None;
            cx.notify();
        });
        self.open = None;
        self.modal = Some(Modal::Status {
            editor,
            _subscription,
        });
        cx.notify();
    }

    /// Closes account action `key`'s dialog, as when the plugin took the
    /// action away.
    pub(crate) fn close_action(&mut self, key: &str, cx: &mut Context<Self>) {
        if matches!(&self.modal, Some(Modal::Plugin(open, _)) if *open == key) {
            self.modal = None;
            cx.notify();
        }
    }

    /// Opens a plugin's account action in its dialog.
    fn open_action(
        &mut self,
        action: &super::account_actions::AccountAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let account = cx.weak_entity();
        let close: super::account_actions::Close = std::rc::Rc::new(move |_, cx| {
            let _ = account.update(cx, |this, cx| {
                this.modal = None;
                cx.notify();
            });
        });
        let view = (action.build)(close, window, cx);
        self.open = None;
        self.modal = Some(Modal::Plugin(action.key, view));
        cx.notify();
    }

    /// "Check for updates", or "Update available" with the new version
    /// once one is on offer; both open Settings → Updates.
    fn update_row(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let c = cx.colors();
        let offered = crate::updates::Updater::get(cx).version();
        let check = offered.is_none();
        let label = match offered {
            Some(_) => "Update available",
            None => "Check for updates",
        };
        let badge = offered.map(|version| {
            div()
                .px(rems(0.375))
                .rounded(radius::FLOATING_INNER)
                .bg(c.affordance_subtle)
                .text_caption()
                .text_color(c.text_subtle)
                .child(version.to_string())
                .into_any_element()
        });
        Self::row("account-updates", IconName::Download, label, badge, cx).on_click(cx.listener(
            move |this, _, window, cx| {
                this.set_open(None, cx);
                if check {
                    crate::updates::check(true, cx);
                }
                window.dispatch_action(Box::new(crate::updates::OpenUpdateSettings), cx);
            },
        ))
    }

    /// A direct menu row: 28px high, 12px gap, `text-body`.
    fn row(
        id: impl Into<ElementId>,
        icon: IconName,
        label: impl Into<SharedString>,
        trailing: Option<AnyElement>,
        cx: &App,
    ) -> Stateful<Div> {
        let c = cx.colors();
        let hover = c.affordance_subtle_hover;
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(rems(0.75))
            .min_h(rems(1.75))
            .px(rems(0.75))
            .py(rems(0.25))
            .rounded(radius::FLOATING_INNER)
            .text_body()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .text_color(c.text_subtle)
                    .child(Icon::new(icon).size(16.)),
            )
            .child(div().flex_1().min_w_0().truncate().child(label.into()))
            .children(trailing)
    }

    fn menu(&self, profile: &Profile, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let (label, tint, text) = match profile.presence {
            Some(PresenceState::Online) => ("Online", Some(c.status_online), c.text_success),
            Some(PresenceState::Away) => ("Away", Some(c.status_away), c.text_warning),
            Some(PresenceState::Offline) => ("Offline", None, c.text_subtle),
            None => ("Status unavailable", None, c.text_subtle),
        };
        let availability_open = self.open == Some(Open::Availability);
        let pending = self
            .availability
            .is_some_and(|change| !change.failed && profile.presence != Some(change.target));
        let chip = div()
            .relative()
            .child(
                div()
                    .id("availability")
                    .flex()
                    .items_center()
                    .h(rems(1.5))
                    .px(rems(0.75))
                    .rounded(radius::CHIP)
                    .text_caption()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(text)
                    .map(|this| match tint {
                        Some(color) => this.bg(color.opacity(0.1)),
                        None => this.bg(c.affordance_selected),
                    })
                    .cursor_pointer()
                    .gap(rems(0.25))
                    .when(pending, |this| this.child(Spinner::new().size(12.)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_open(
                            Some(if availability_open {
                                Open::Menu
                            } else {
                                Open::Availability
                            }),
                            cx,
                        )
                    })),
            )
            .when(availability_open, |this| {
                let mode = cx.global::<AppState>().presence_mode;
                let item = |id: &'static str, label: &'static str, value: PresenceMode| {
                    MenuItem::new(id, label)
                        .checked(mode == value)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            AppState::set_presence_mode(value, cx);
                            this.confirm_availability(value, cx);
                            this.set_open(Some(Open::Menu), cx);
                        }))
                };
                this.child(overlay::dropdown(
                    window,
                    Align::Start,
                    Menu::new()
                        .child(item("availability-online", "Online", PresenceMode::Auto))
                        .child(item("availability-away", "Away", PresenceMode::Away))
                        .child(item(
                            "availability-offline",
                            "Offline",
                            PresenceMode::Offline,
                        )),
                    Self::dismiss(cx, Some(Open::Menu)),
                ))
            });

        let header = div()
            .flex()
            .items_center()
            .gap(rems(0.75))
            .py(rems(0.5))
            .px(rems(0.25))
            .min_w_0()
            .child(div().flex_none().size(rems(2.5)).map(|this| {
                match self.on_profile.clone() {
                    Some(on_profile) => this.child(
                        IconButton::new("view-profile", IconName::User)
                            .size(IconButtonSize::Md)
                            .variant(ButtonVariant::Avatar)
                            .shape(IconButtonShape::Round)
                            .tooltip("View your profile")
                            .child(profile.avatar(AvatarSize::Px(40.), false))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_open(None, cx);
                                on_profile(window, cx);
                            })),
                    ),
                    None => this.child(profile.avatar(AvatarSize::Px(40.), false)),
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(rems(0.25))
                    .min_w_0()
                    .child(div().text_body().truncate().child(profile.name.clone()))
                    .when(profile.community, |this| this.child(chip)),
            );

        let status = self.current_status(cx);
        let status_row = profile.community.then(|| {
            let hover = c.affordance_subtle_hover;
            let content: Vec<AnyElement> = match &status {
                Some(status) => AppState::current(cx)
                    .and_then(|community| {
                        super::status::emoji(&status.emoji, px(20.), &community, cx)
                    })
                    .into_iter()
                    .chain([div()
                        .min_w_0()
                        .truncate()
                        .text_body_sm()
                        .child(status.text.clone())
                        .into_any_element()])
                    .collect(),
                None => vec![
                    Icon::new(IconName::Smiley).size(16.).into_any_element(),
                    div()
                        .child(if self.status_loading && self.status.is_none() {
                            "Loading status…"
                        } else {
                            "Set a status"
                        })
                        .into_any_element(),
                ],
            };
            div()
                .my(rems(0.75))
                .bg(c.surface_inset)
                .border_1()
                .border_color(c.border_standard)
                .rounded(radius::CONTROL)
                .child(
                    div()
                        .id("set-status")
                        .flex()
                        .items_center()
                        .gap(rems(0.5))
                        .min_h(rems(2.5))
                        .px(rems(0.75))
                        .rounded(rems(0.6875))
                        .text_body()
                        .text_color(c.text_subtle)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .children(content)
                        .on_click(cx.listener(|this, _, window, cx| this.edit_status(window, cx))),
                )
        });
        let error = |text: SharedString| {
            div()
                .mx(rems(0.75))
                .my(rems(0.5))
                .max_w(rems(14.))
                .text_body_sm()
                .text_color(c.text_danger)
                .child(text)
        };
        let settings_check = self.settings_open.then(|| {
            Icon::new(IconName::Check)
                .size(14.)
                .color(c.text_subtle)
                .into_any_element()
        });
        let presence_error = cx.global::<AppState>().presence_error.clone();
        div()
            .mt(px(4.))
            .w(rems(18.))
            .p(rems(0.5))
            .flex()
            .flex_col()
            .floating_surface(&c)
            .child(header)
            .when(
                self.availability.is_some_and(|change| change.failed),
                |this| this.child(error("Could not confirm your status. Try again.".into())),
            )
            .children(presence_error.map(|text| error(text.into())))
            .children(self.status_error.clone().map(error))
            .children(status_row)
            .child(MenuSeparator::new())
            .children(
                super::account_actions::all(cx)
                    .into_iter()
                    .map(|action| {
                        let id = SharedString::from(format!("account-action-{}", action.key));
                        let row = Self::row(id, action.icon, action.title.clone(), None, cx);
                        row.on_click(cx.listener(move |this, _, window, cx| {
                            this.open_action(&action, window, cx)
                        }))
                    })
                    .collect::<Vec<_>>(),
            )
            .child(self.update_row(cx))
            .child(
                Self::row(
                    "account-settings",
                    IconName::Gear,
                    "Settings",
                    settings_check,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.set_open(None, cx);
                    window.dispatch_action(Box::new(OpenSettings), cx);
                })),
            )
            .into_any_element()
    }

    fn modal(&self, window: &Window) -> Option<AnyElement> {
        let content = match self.modal.as_ref()? {
            Modal::Status { editor, .. } => editor.clone().into_any_element(),
            Modal::Plugin(_, view) => view.clone().into_any_element(),
        };
        Some(overlay::modal(window, content, None).into_any_element())
    }
}

/// What the menu shows about the viewer.
struct Profile {
    name: SharedString,
    /// The profile has a display name; otherwise the avatar shows a person
    /// glyph instead of an initial.
    named: bool,
    picture: Option<ImageSource>,
    presence: Option<PresenceState>,
    /// A community is selected, so status and availability apply.
    community: bool,
}

impl Profile {
    fn read(cx: &App) -> Self {
        let state = cx.global::<AppState>();
        let viewer = match &state.identity {
            IdentityState::Ready(identity) => Some(identity.pubkey_hex()),
            _ => None,
        };
        let community = AppState::current(cx);
        let snapshot = community
            .as_ref()
            .map(|community| community.read(cx).snapshot.clone());
        let profile = viewer
            .as_ref()
            .zip(snapshot.as_ref())
            .and_then(|(viewer, snapshot)| snapshot.profiles.get(viewer).cloned());
        // Without a community, the default profile new communities start
        // from (`ProfileButton.tsx`).
        let local = community
            .is_none()
            .then(|| super::settings::default_profile(cx));
        let display_name = profile
            .as_ref()
            .filter(|profile| !profile.name_is_fallback && !profile.name.trim().is_empty())
            .map(|profile| profile.name.clone())
            .or_else(|| {
                local
                    .as_ref()
                    .map(|local| local.name.trim().to_string())
                    .filter(|name| !name.is_empty())
            });
        let named = display_name.is_some();
        let name = display_name
            .or_else(|| viewer.as_ref().map(|viewer| short_key(viewer)))
            .unwrap_or_else(|| "Your profile".into());
        let picture = profile
            .as_ref()
            .and_then(|profile| profile.picture.clone())
            .zip(community.as_ref())
            .map(|(picture, community)| media::avatar(&picture, community.read(cx).session.media()))
            .or_else(|| {
                local
                    .as_ref()
                    .filter(|local| local.picture.starts_with("https://"))
                    .map(|local| ImageSource::from(SharedString::from(local.picture.clone())))
            });
        let presence = viewer
            .as_ref()
            .zip(snapshot.as_ref())
            .and_then(|(viewer, snapshot)| snapshot.presence.get(viewer).copied());
        Self {
            name: name.into(),
            named,
            picture,
            presence,
            community: community.is_some() && viewer.is_some(),
        }
    }

    fn avatar(&self, size: AvatarSize, disc: bool) -> Avatar {
        let mut avatar = Avatar::new(self.name.clone()).size(size);
        if disc {
            avatar = avatar.profile_disc();
        }
        if !self.named {
            avatar = avatar.fallback_icon(IconName::User, 19.);
        }
        if let Some(picture) = &self.picture {
            avatar = avatar.image(picture.clone());
        }
        avatar
    }
}

/// `formatPublicKey`: the first and last characters of a hex key.
fn short_key(key: &str) -> String {
    if key.len() > 16 {
        format!("{}…{}", &key[..8], &key[key.len() - 4..])
    } else {
        key.to_string()
    }
}

impl Render for Account {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_profile(cx);
        let profile = Profile::read(cx);
        // Confirmed: the relay shows what the user chose.
        if self
            .availability
            .is_some_and(|change| profile.presence == Some(change.target))
        {
            self.availability = None;
        }
        let presence = profile.presence.map(|state| match state {
            PresenceState::Online => Presence::Online,
            PresenceState::Away => Presence::Away,
            PresenceState::Offline => Presence::Offline,
        });
        let menu_open = self.open.is_some();
        let menu = menu_open.then(|| self.menu(&profile, window, cx));
        let modal = self.modal(window);
        div()
            .relative()
            .child(
                IconButton::new("me", IconName::User)
                    .size(IconButtonSize::Sm)
                    .variant(ButtonVariant::Avatar)
                    .shape(IconButtonShape::Round)
                    .tooltip(profile.name.clone())
                    .child(
                        profile
                            .avatar(AvatarSize::Px(32.), true)
                            .presence(presence)
                            .cutout_color(cx.colors().surface_base),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_open((!menu_open).then_some(Open::Menu), cx)
                    })),
            )
            .when_some(menu, |this, menu| {
                this.child(overlay::dropdown(
                    window,
                    Align::End,
                    menu,
                    Self::dismiss(cx, None),
                ))
            })
            .children(modal)
    }
}
