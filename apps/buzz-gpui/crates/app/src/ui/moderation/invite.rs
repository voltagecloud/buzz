//! "Add or invite people" (`InviteDialog` in `CommunityAdmin.tsx`): add
//! someone by their exact public identity, or share an invite link whose
//! expiry and use limit can be chosen. Opening it mints the link.

use super::card::{CommunityAdmin, STALE};
use super::store;
use crate::community::AppState;
use crate::ui::modal::Dismissed;
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::moderation::{Change, Grant, Member, Role, parse_public_key};
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, Dialog, IconName, Menu, MenuItem, MenuSize, Surface,
    TypeStyled, tokens::radius,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;
use std::time::Duration;

const DAY: u64 = 24 * 60 * 60;
/// "Expires after": seconds and label.
const EXPIRY: [(u64, &str); 4] = [
    (DAY, "1 day"),
    (3 * DAY, "3 days"),
    (7 * DAY, "7 days"),
    (30 * DAY, "30 days"),
];
/// "Limit number of uses".
const USES: [(Option<u32>, &str); 5] = [
    (None, "No limit"),
    (Some(1), "1 use"),
    (Some(5), "5 uses"),
    (Some(10), "10 uses"),
    (Some(25), "25 uses"),
];
const ROLES: [(Grant, &str); 2] = [(Grant::Member, "Member"), (Grant::Admin, "Admin")];
const COPIED_FOR: Duration = Duration::from_secs(2);

/// The settings one link is minted for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Settings {
    ttl: u64,
    uses: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ttl: 3 * DAY,
            uses: None,
        }
    }
}

/// The link for one set of settings: minting, minted or failed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Link {
    settings: Settings,
    url: Option<String>,
    failed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Role,
    Expiry,
    Uses,
}

/// The identity field's key, or why it cannot be added.
pub(super) fn direct_key(input: &str, members: &[Member]) -> Option<Result<String, String>> {
    if input.trim().is_empty() {
        return None;
    }
    Some(parse_public_key(input).and_then(|key| {
        if members.iter().any(|member| member.pubkey == key) {
            Err("This person is already a community member.".into())
        } else {
            Ok(key)
        }
    }))
}

pub struct InviteDialog {
    card: WeakEntity<CommunityAdmin>,
    settings: Settings,
    identity: Entity<InputState>,
    role: Grant,
    adding: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    /// "Copied" shows for this copy until it times out.
    copied: Option<usize>,
    copies: usize,
    link: Option<Link>,
    /// The mint whose answer counts.
    minting: u64,
    /// Settings already asked for, so one open never mints twice.
    requested: Option<Settings>,
    choice: Option<Choice>,
    _subscriptions: [Subscription; 2],
}

impl EventEmitter<Dismissed> for InviteDialog {}

impl InviteDialog {
    pub fn new(
        card: WeakEntity<CommunityAdmin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let identity = cx.new(|cx| InputState::new(window, cx).placeholder("npub1… or public key"));
        let subscriptions = [
            store::observe(cx, |this, cx| this.sync(cx)),
            cx.subscribe_in(
                &identity,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.add(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
        ];
        identity.update(cx, |state, cx| state.focus(window, cx));
        let mut dialog = Self {
            card,
            settings: Settings::default(),
            identity,
            role: Grant::Member,
            adding: false,
            error: None,
            notice: None,
            copied: None,
            copies: 0,
            link: None,
            minting: 0,
            requested: None,
            choice: None,
            _subscriptions: subscriptions,
        };
        dialog.sync(cx);
        dialog
    }

    /// A direct addition is running; the dialog cannot close.
    pub fn adding(&self) -> bool {
        self.adding
    }

    fn current(&self) -> Option<&Link> {
        self.link
            .as_ref()
            .filter(|link| link.settings == self.settings)
    }

    fn locked(cx: &App) -> bool {
        let snapshot = store::snapshot(cx);
        snapshot.error.is_some() || snapshot.refreshing
    }

    /// Opening the dialog, or changing its settings, mints the link to
    /// share, once per settings and only from a current roster.
    fn sync(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        if Self::locked(cx) || self.current().is_some() || self.requested == Some(self.settings) {
            return;
        }
        let Some(community) = AppState::current(cx) else {
            return;
        };
        let settings = self.settings;
        self.requested = Some(settings);
        self.minting += 1;
        let request = self.minting;
        self.link = Some(Link {
            settings,
            url: None,
            failed: false,
        });
        self.copied = None;
        self.error = None;
        let session = community.read(cx).session.clone();
        cx.spawn(async move |this, cx| {
            let result = session.mint_invite(settings.ttl, settings.uses).await;
            let _ = this.update(cx, |this, cx| {
                if this.minting != request {
                    return;
                }
                match result {
                    Ok(invite) => {
                        this.link = Some(Link {
                            settings,
                            url: Some(invite.url),
                            failed: false,
                        })
                    }
                    Err(error) => {
                        this.requested = None;
                        this.link = Some(Link {
                            settings,
                            url: None,
                            failed: true,
                        });
                        this.error = Some(error.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = store::snapshot(cx);
        let members = snapshot.members.clone().unwrap_or_default();
        let input = self.identity.read(cx).value().to_string();
        let Some(Ok(pubkey)) = direct_key(&input, &members) else {
            return;
        };
        if Self::locked(cx) || self.adding {
            return;
        }
        let role = if snapshot.role == Some(Role::Owner) {
            self.role
        } else {
            Grant::Member
        };
        let Ok(task) = self
            .card
            .update(cx, |card, cx| card.apply(Change::Add { pubkey, role }, cx))
        else {
            return;
        };
        self.adding = true;
        self.error = None;
        self.notice = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.adding = false;
                match result {
                    Ok(()) => {
                        this.identity
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        this.role = Grant::Member;
                        this.notice = Some("Member added directly.".into());
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_or_retry(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.current().cloned() else {
            return;
        };
        if link.failed {
            self.link = None;
            self.sync(cx);
            return;
        }
        let Some(url) = link.url else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        self.copies += 1;
        let copy = self.copies;
        self.copied = Some(copy);
        self.notice = Some("Invite link copied.".into());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied == Some(copy) {
                    this.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.adding {
            cx.emit(Dismissed);
        }
    }

    fn dismiss_choice(cx: &mut Context<Self>) -> Dismiss {
        let weak = cx.entity().downgrade();
        Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.choice = None;
                cx.notify();
            });
        })
    }

    /// A label-left, value-right choice (`Choice`): a ghost button with the
    /// value that opens a radio menu.
    #[allow(clippy::too_many_arguments)]
    fn choice<T: Copy + PartialEq + 'static>(
        &self,
        choice: Choice,
        tooltip: &'static str,
        value: T,
        options: &'static [(T, &'static str)],
        disabled: bool,
        set: fn(&mut Self, T),
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.choice == Some(choice);
        let label = options
            .iter()
            .find(|(option, _)| *option == value)
            .map_or("", |(_, label)| label);
        let menu = open.then(|| {
            let mut menu = Menu::new().size(MenuSize::Compact);
            for (index, (option, label)) in options.iter().enumerate() {
                let option = *option;
                menu = menu.child(
                    MenuItem::new(SharedString::from(format!("{tooltip}-{index}")), *label)
                        .size(MenuSize::Compact)
                        .checked(option == value)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.choice = None;
                            set(this, option);
                            this.sync(cx);
                        })),
                );
            }
            overlay::dropdown(window, Align::End, menu, Self::dismiss_choice(cx))
        });
        div()
            .relative()
            .flex_none()
            .child(
                Button::new(SharedString::from(tooltip), label)
                    .ghost()
                    .size(ButtonSize::Sm)
                    .surface(Surface::Floating)
                    .icon_end(IconName::CaretDown)
                    .disabled(disabled)
                    .when(!open, |b| b.tooltip(tooltip))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choice = if this.choice == Some(choice) {
                            None
                        } else {
                            Some(choice)
                        };
                        cx.notify();
                    })),
            )
            .children(menu)
            .into_any_element()
    }
}

/// `InputGroup`: one inset frame around a field and its controls.
fn input_group(focused: bool, invalid: bool, cx: &App) -> Div {
    let c = cx.colors();
    div()
        .flex()
        .items_center()
        .gap(rems(0.5))
        .w_full()
        .min_w_0()
        .min_h(rems(2.5))
        .px(rems(1.))
        .rounded(radius::CONTROL)
        .border_1()
        .border_color(if invalid {
            c.border_danger
        } else if focused {
            c.interaction(Surface::Floating).border
        } else {
            gpui_kit::transparent_black()
        })
        .bg(c.surface_inset)
        .text_body()
        .text_color(c.text_subtle)
}

impl Render for InviteDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let snapshot = store::snapshot(cx);
        let members = snapshot.members.clone().unwrap_or_default();
        let owner = snapshot.role == Some(Role::Owner);
        let stale = snapshot.error.is_some();
        let locked = stale || snapshot.refreshing;
        let input = self.identity.read(cx).value().to_string();
        let checked = direct_key(&input, &members);
        let identity_error = checked.as_ref().and_then(|result| result.clone().err());
        let can_add = matches!(checked, Some(Ok(_))) && !locked && !self.adding;
        let focused = self
            .identity
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);

        let current = self.current().cloned();
        let url = current.as_ref().and_then(|link| link.url.clone());
        let failed = current.as_ref().is_some_and(|link| link.failed);
        let minting = current.is_some() && url.is_none() && !failed;
        let placeholder = if failed {
            "Couldn’t create invite link"
        } else if locked && current.is_none() {
            "Retry the member list to create a link"
        } else {
            "Creating invite link…"
        };

        let heading = |text: &'static str| div().text_label().child(text);
        let hint = |text: &'static str| {
            div()
                .mt(rems(0.25))
                .mb(rems(0.75))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
        };

        let role_choice = owner.then(|| {
            self.choice(
                Choice::Role,
                "Choose member role",
                self.role,
                &ROLES,
                self.adding,
                |this, role| this.role = role,
                window,
                cx,
            )
        });
        let add_directly = div()
            .flex()
            .flex_col()
            .child(heading("Add directly"))
            .child(hint("Paste the person’s npub or 64-character public key."))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(rems(0.5))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                input_group(focused, identity_error.is_some(), cx)
                                    .child(
                                        div().flex_1().min_w_0().text_color(c.text_standard).child(
                                            Input::new(&self.identity)
                                                .appearance(false)
                                                .disabled(self.adding)
                                                .px_0()
                                                .py_0()
                                                .h(rems(1.25)),
                                        ),
                                    )
                                    .children(role_choice),
                            )
                            .children(
                                identity_error
                                    .map(|error| div().mt(rems(0.5)).text_body_sm().child(error)),
                            ),
                    )
                    .child(
                        Button::new("membership-add", "Add member")
                            .prominent()
                            .loading(self.adding)
                            .disabled(!can_add)
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            );

        let expiry = self.choice(
            Choice::Expiry,
            "Choose invite expiry",
            self.settings.ttl,
            &EXPIRY,
            minting || locked,
            |this, ttl| this.settings.ttl = ttl,
            window,
            cx,
        );
        let uses = self.choice(
            Choice::Uses,
            "Choose maximum invite uses",
            self.settings.uses,
            &USES,
            minting || locked,
            |this, uses| this.settings.uses = uses,
            window,
            cx,
        );
        let row = |label: &'static str, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(rems(1.))
                .child(div().text_label_sm().child(label))
                .child(control)
        };
        let link_button = Button::new(
            "membership-copy-link",
            if failed {
                "Retry"
            } else if self.copied.is_some() {
                "Copied"
            } else {
                "Copy link"
            },
        )
        .size(ButtonSize::Sm)
        .surface(Surface::Floating)
        .loading(minting)
        .disabled(if failed { locked } else { url.is_none() })
        .on_click(cx.listener(|this, _, _, cx| this.copy_or_retry(cx)));
        let invite_link = div()
            .flex()
            .flex_col()
            .child(heading("Invite with a link"))
            .child(hint(
                "Anyone with this link can use it to join until it expires.",
            ))
            .child(
                input_group(false, false, cx)
                    .pr(rems(0.1875))
                    .child(
                        div()
                            .id("membership-invite-link")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .map(|this| match &url {
                                Some(url) => this.text_color(c.text_standard).child(url.clone()),
                                None => this.child(placeholder),
                            }),
                    )
                    .child(link_button),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.25))
                    .child(row("Expires after", expiry))
                    .child(row("Limit number of uses", uses)),
            )
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body_sm().child(error)),
            )
            .when(stale && !self.adding, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(rems(0.75))
                        .child(div().flex_1().min_w_0().text_body_sm().child(STALE))
                        .child(
                            Button::new("membership-dialog-retry", "Retry")
                                .size(ButtonSize::Sm)
                                .surface(Surface::Floating)
                                .loading(snapshot.refreshing)
                                .disabled(snapshot.refreshing)
                                .on_click(cx.listener(|_, _, _, cx| store::reload(cx))),
                        ),
                )
            })
            .children(
                self.notice
                    .clone()
                    .map(|notice| div().text_body_sm().child(notice)),
            );

        Dialog::new("add-or-invite-people", "Add or invite people")
            .description(
                "Add someone by their exact identity, or share a link they can use to join.",
            )
            .on_close(cx.listener(|this, _, _, cx| this.close(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.5))
                    .text_color(c.text_standard)
                    .child(add_directly)
                    .child(invite_link),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn checks_the_identity_against_the_roster() {
        let member = Member {
            pubkey: "c".repeat(64),
            role: Role::Member,
        };
        let members = [member];
        assert_eq!(direct_key("  ", &members), None);
        assert_eq!(
            direct_key("Carol", &members),
            Some(Err("Enter an npub or 64-character hex public key.".into()))
        );
        assert_eq!(
            direct_key(&"C".repeat(64), &members),
            Some(Err("This person is already a community member.".into()))
        );
        assert_eq!(
            direct_key(&"d".repeat(64), &members),
            Some(Ok("d".repeat(64)))
        );
    }

    #[test]
    fn options_match_the_reference() {
        assert_eq!(Settings::default().ttl, 259_200);
        assert_eq!(Settings::default().uses, None);
        let labels: Vec<_> = EXPIRY.iter().map(|(_, l)| *l).collect();
        assert_eq!(labels, ["1 day", "3 days", "7 days", "30 days"]);
        let labels: Vec<_> = USES.iter().map(|(_, l)| *l).collect();
        assert_eq!(
            labels,
            ["No limit", "1 use", "5 uses", "10 uses", "25 uses"]
        );
    }
}
