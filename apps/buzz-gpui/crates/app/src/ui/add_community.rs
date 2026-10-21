//! "Add a community" (relay.md §1.6–1.7, `CommunityDialog.tsx`): a relay
//! address opens a community the identity already belongs to; an invite
//! link (`buzz://join?relay=…&code=…` or `https://host/invite/<code>`)
//! shows the community's join policy when it has one, then claims the
//! invite. Either way the last step is the viewer's profile there, which
//! is published unless it is already current.
//!
//! The join journal records each step before it talks to the relay, so a
//! join interrupted after the claim resumes at the profile step instead of
//! claiming the invite again.

use super::modal::{Dismissed, error};
use crate::community::{AppState, IdentityState};
use crate::runtime::Runtime;
use buzz_relay_client::community::{
    self, CommunityInput, CommunityProfile, JoinInfo, JoinJournal, JoinPolicy, PendingJoin,
};
use buzz_relay_client::{ProfileEdit, RelayHttp, RelayOrigin, account};
use buzz_ui::{ActiveTheme, Button, Checkbox, Dialog, Surface, TextArea, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

enum Step {
    Address,
    /// The invite's community has a join policy to accept first.
    Policy {
        origin: RelayOrigin,
        code: String,
        name: Option<String>,
        policy: JoinPolicy,
    },
    /// How the viewer appears in the community.
    Profile {
        origin: RelayOrigin,
        /// The community's NIP-11 name, or its host.
        name: String,
        /// The kind 0 there now, if any.
        found: CommunityProfile,
    },
}

/// What a check or claim leads to.
enum Next {
    /// A member: on to the profile step.
    Profile(CommunityProfile),
    NotMember(JoinInfo),
}

pub struct AddCommunity {
    step: Step,
    field: Entity<InputState>,
    display_name: Entity<InputState>,
    about: Entity<TextareaState>,
    /// The loaded profile's picture, carried through unchanged: buzz-app's
    /// join profile has no field for it.
    picture: Entity<InputState>,
    agreed: bool,
    age_confirmed: bool,
    busy: bool,
    error: Option<SharedString>,
    /// Shown inside onboarding rather than as a dismissible dialog.
    embedded: bool,
    journal: Option<JoinJournal>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Dismissed> for AddCommunity {}

impl AddCommunity {
    pub fn new(embedded: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = cx.new(|cx| InputState::new(window, cx).placeholder("wss://relay.example.com"));
        field.update(cx, |state, cx| state.focus(window, cx));
        let display_name = cx.new(|cx| InputState::new(window, cx));
        let about = cx.new(|cx| TextareaState::new(window, cx));
        let picture = cx.new(|cx| InputState::new(window, cx).placeholder("https://"));
        let subscriptions = vec![
            cx.subscribe_in(&field, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.next(window, cx);
                }
            }),
            cx.subscribe(&display_name, Self::edited),
            cx.subscribe(&about, Self::edited),
            cx.subscribe(&picture, Self::edited),
        ];
        let journal = match &cx.global::<AppState>().identity {
            IdentityState::Ready(identity) => Some(JoinJournal::new(
                cx.global::<AppState>().store.clone(),
                identity.pubkey_hex(),
            )),
            _ => None,
        };
        // An unfinished join picks up where it stopped.
        let mut error = None;
        match journal.as_ref().map(JoinJournal::latest) {
            Some(Ok(Some(pending))) => field.update(cx, |state, cx| {
                state.set_value(pending.community, window, cx);
            }),
            Some(Err(message)) => error = Some(message.into()),
            _ => {}
        }
        Self {
            step: Step::Address,
            field,
            display_name,
            about,
            picture,
            agreed: false,
            age_confirmed: false,
            busy: false,
            error,
            embedded,
            journal,
            _subscriptions: subscriptions,
        }
    }

    fn edited<T>(&mut self, _: Entity<T>, event: &InputEvent, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::Change) {
            self.error = None;
            cx.notify();
        }
    }

    /// Fills in `link` and checks it, as Continue would.
    pub fn submit(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.field
            .update(cx, |state, cx| state.set_value(link.to_owned(), window, cx));
        self.next(window, cx);
    }

    fn http(origin: &RelayOrigin, cx: &App) -> Result<RelayHttp, String> {
        match &cx.global::<AppState>().identity {
            IdentityState::Ready(identity) => Ok(RelayHttp::new(origin.clone(), identity.clone())),
            _ => Err("Set up your identity first".into()),
        }
    }

    fn journal(&self) -> Result<JoinJournal, String> {
        self.journal
            .clone()
            .ok_or_else(|| "Set up your identity first".into())
    }

    /// Continue: check the relay, then go to the profile step for a
    /// member, or show the policy or claim the invite for a newcomer.
    fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let input = match community::parse_input(&self.field.read(cx).value()) {
            Ok(input) => input,
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
                return;
            }
        };
        let http = match Self::http(input.origin(), cx) {
            Ok(http) => http,
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
                return;
            }
        };
        let pending = match self
            .journal()
            .and_then(|journal| journal.get(&input.origin().to_string()))
        {
            Ok(pending) => pending,
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.error = None;
        let runtime = Runtime::handle(cx);
        cx.spawn_in(window, async move |this, cx| {
            let work = runtime.spawn(async move {
                let info = community::inspect(&http).await?;
                let found = community::inspect_profile(&http).await?;
                Ok::<_, String>(match found {
                    Some(found) => (info, Next::Profile(found)),
                    None => (info.clone(), Next::NotMember(info)),
                })
            });
            let result = work
                .await
                .unwrap_or_else(|_| Err("The check stopped.".into()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match (result, input) {
                    (Err(message), _) => this.error = Some(message.into()),
                    (Ok((info, Next::Profile(found))), input) => {
                        let origin = input.origin().clone();
                        this.show_profile(origin, info.name, found, pending, window, cx);
                    }
                    (Ok(_), CommunityInput::Relay(_)) => {
                        this.error = Some(
                            "You’re not a member of this community yet. Paste an invite link \
                             to join."
                                .into(),
                        )
                    }
                    (Ok((_, Next::NotMember(info))), CommunityInput::Invite { origin, code }) => {
                        match info.policy {
                            Some(policy) => {
                                this.step = Step::Policy {
                                    origin,
                                    code,
                                    name: info.name,
                                    policy,
                                };
                            }
                            None => this.claim(origin, code, info.name, None, window, cx),
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Journals the join, then claims the invite. A join already journaled
    /// may have been admitted by a claim whose answer was lost, so that is
    /// read first and the invite is not claimed again.
    fn claim(
        &mut self,
        origin: RelayOrigin,
        code: String,
        name: Option<String>,
        policy: Option<JoinPolicy>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (http, journal) = match Self::http(&origin, cx).and_then(|h| Ok((h, self.journal()?))) {
            Ok(pair) => pair,
            Err(message) => {
                self.error = Some(message.into());
                return;
            }
        };
        self.busy = true;
        self.error = None;
        let age = self.age_confirmed;
        let runtime = Runtime::handle(cx);
        let community = origin.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let work = runtime.spawn(async move {
                if let Some(pending) = journal.get(&community)?
                    && let Some(found) = community::inspect_profile(&http).await?
                {
                    return Ok((found, pending));
                }
                let step = journal.begin(&community, None)?;
                community::claim(&http, &code, policy.as_ref(), age).await?;
                let found = community::inspect_profile(&http)
                    .await?
                    .ok_or("Membership was not confirmed")?;
                Ok::<_, String>((found, step))
            });
            let result = work
                .await
                .unwrap_or_else(|_| Err("The join stopped.".into()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok((found, step)) => {
                        this.show_profile(origin, name, found, Some(step), window, cx)
                    }
                    Err(message) => this.error = Some(message.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The profile step, filled from the journaled draft, else the
    /// community profile, else the local default (`showProfile`).
    fn show_profile(
        &mut self,
        origin: RelayOrigin,
        name: Option<String>,
        found: CommunityProfile,
        pending: Option<PendingJoin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile = pending
            .and_then(|pending| pending.profile)
            .or_else(|| found.profile.clone())
            .or_else(|| default_profile(cx))
            .unwrap_or_default();
        self.display_name
            .update(cx, |state, cx| state.set_value(profile.name, window, cx));
        self.about
            .update(cx, |state, cx| state.set_value(profile.about, window, cx));
        self.picture
            .update(cx, |state, cx| state.set_value(profile.picture, window, cx));
        self.display_name
            .update(cx, |state, cx| state.focus(window, cx));
        let name = name.unwrap_or_else(|| {
            url::Url::parse(&origin.http())
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| origin.to_string())
        });
        self.step = Step::Profile {
            origin,
            name,
            found,
        };
        self.error = None;
        cx.notify();
    }

    fn profile(&self, cx: &App) -> ProfileEdit {
        ProfileEdit {
            name: self.display_name.read(cx).value().trim().to_string(),
            picture: self.picture.read(cx).value().trim().to_string(),
            about: self.about.read(cx).value().trim().to_string(),
        }
    }

    /// Publishes the profile unless it is already current, confirms it by
    /// reading it back, then saves the community and ends the join.
    fn publish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Step::Profile { origin, .. } = &self.step else {
            return;
        };
        let origin = origin.clone();
        let profile = self.profile(cx);
        if self.busy || profile.name.is_empty() {
            return;
        }
        let (http, journal) = match Self::http(&origin, cx).and_then(|h| Ok((h, self.journal()?))) {
            Ok(pair) => pair,
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.error = None;
        let runtime = Runtime::handle(cx);
        let draft = profile.clone();
        let community = origin.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let work = runtime.spawn(async move {
                let step = journal.begin(&community, Some(draft.clone()))?;
                let found = community::inspect_profile(&http)
                    .await?
                    .ok_or("Membership was not confirmed")?;
                if !found.matches(&draft) {
                    community::publish_profile(&http, &draft, &found).await?;
                    // An accepted replaceable event may already be superseded.
                    let confirmed = community::inspect_profile(&http).await?;
                    if !confirmed.is_some_and(|confirmed| confirmed.matches(&draft)) {
                        return Err("Your profile change is not current. Your edits are \
                                    retained; try again."
                            .to_owned());
                    }
                }
                Ok::<_, String>(step)
            });
            let result = work
                .await
                .unwrap_or_else(|_| Err("The join stopped.".into()));
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(step) => this.finish(&origin, &profile, &step, cx),
                    Err(message) => this.error = Some(message.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn finish(
        &mut self,
        origin: &RelayOrigin,
        profile: &ProfileEdit,
        step: &PendingJoin,
        cx: &mut Context<Self>,
    ) {
        if let Err(message) = AppState::add_community(origin.websocket(), cx) {
            self.error = Some(message.into());
            return;
        }
        if let Ok(journal) = self.journal()
            && let Err(message) = journal.finish(step)
        {
            tracing::warn!(%message, "could not close the join journal");
        }
        seed_default_profile(profile, origin, cx);
        cx.emit(Dismissed);
    }

    fn accept(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Step::Policy {
            origin,
            code,
            name,
            policy,
        } = &self.step
        else {
            return;
        };
        if !self.agreed || (policy.age_required && !self.age_confirmed) {
            self.error = Some("Accept the community's terms to join.".into());
            cx.notify();
            return;
        }
        let (origin, code, name, policy) =
            (origin.clone(), code.clone(), name.clone(), policy.clone());
        self.claim(origin, code, name, Some(policy), window, cx);
    }
}

/// The default profile Settings keeps, if one was saved.
fn default_profile(cx: &App) -> Option<ProfileEdit> {
    let profile = super::settings::default_profile(cx);
    (profile != ProfileEdit::default()).then_some(profile)
}

/// Seeds the default from the first community profile, as
/// `communities.joined` does.
fn seed_default_profile(profile: &ProfileEdit, origin: &RelayOrigin, cx: &App) {
    if default_profile(cx).is_some_and(|saved| !saved.name.is_empty()) {
        return;
    }
    super::settings::remember_profile(profile.clone(), Some(origin), cx);
}

impl Render for AddCommunity {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let embedded = self.embedded;
        let title = match &self.step {
            Step::Profile { name, .. } => format!("Your profile in {name}"),
            _ => "Add a community".to_owned(),
        };
        let mut dialog = Dialog::new("add-community", title)
            .when(matches!(self.step, Step::Address), |d| {
                d.description(
                    "Use your identity across communities. Your profile and conversations \
                     stay separate in each one.",
                )
            })
            .when(!embedded, |d| {
                d.on_close(cx.listener(|_, _, _, cx| cx.emit(Dismissed)))
            });
        let relay_note = |origin: &RelayOrigin| {
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(format!("Relay: {}", origin.http()))
        };
        let back = |id: &'static str, cx: &mut Context<Self>| {
            Button::new(id, "Back")
                .ghost()
                .surface(Surface::Floating)
                .disabled(self.busy)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.step = Step::Address;
                    this.error = None;
                    this.agreed = false;
                    this.age_confirmed = false;
                    cx.notify();
                }))
        };
        match &self.step {
            Step::Address => {
                dialog = dialog
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(0.5))
                            .child(div().text_label_sm().child("Relay URL or invite link"))
                            .child(TextField::new(&self.field).surface(Surface::Floating))
                            .child(div().text_body_sm().text_color(c.text_subtle).child(
                                "Enter a wss:// or https:// relay address without a path, or \
                                     paste an invite link. Continue contacts this relay using \
                                     your Buzz identity.",
                            ))
                            .children(self.error.clone().map(|message| error(message, cx))),
                    )
                    .when(!embedded, |d| {
                        d.footer_leading(
                            Button::new("community-back", "Back")
                                .ghost()
                                .surface(Surface::Floating)
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismissed))),
                        )
                    })
                    .footer(
                        Button::new("community-continue", "Continue")
                            .prominent()
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.next(window, cx))),
                    );
            }
            Step::Policy {
                origin,
                name,
                policy,
                ..
            } => {
                let documents = [("Terms", &policy.terms), ("Privacy", &policy.privacy)];
                let mut body = div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .child(relay_note(origin))
                    .child(div().text_body_sm().child(format!(
                        "{} asks new members to accept its terms before joining.",
                        name.as_deref().unwrap_or("This community")
                    )));
                for (label, text) in documents {
                    if let Some(text) = text {
                        body = body.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(rems(0.5))
                                .child(div().text_label_sm().child(label))
                                .child(
                                    div()
                                        .id(label)
                                        .max_h(px(160.))
                                        .overflow_y_scroll()
                                        .p(rems(0.75))
                                        .rounded(px(12.))
                                        .bg(c.surface_inset)
                                        .text_body_sm()
                                        .child(text.clone()),
                                ),
                        );
                    }
                }
                let weak = cx.entity().downgrade();
                body = body.child(
                    Checkbox::new("accept-terms", self.agreed)
                        .label("I accept this community's terms")
                        .surface(Surface::Floating)
                        .on_change(move |checked, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.agreed = checked;
                                cx.notify();
                            });
                        }),
                );
                if policy.age_required {
                    let weak = cx.entity().downgrade();
                    body = body.child(
                        Checkbox::new("confirm-age", self.age_confirmed)
                            .label("I confirm I meet this community's minimum age")
                            .surface(Surface::Floating)
                            .on_change(move |checked, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.age_confirmed = checked;
                                    cx.notify();
                                });
                            }),
                    );
                }
                body = body.children(self.error.clone().map(|message| error(message, cx)));
                dialog = dialog
                    .child(body)
                    .footer_leading(back("policy-back", cx))
                    .footer(
                        Button::new("policy-join", "Join community")
                            .prominent()
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.accept(window, cx))),
                    );
            }
            Step::Profile { origin, found, .. } => {
                let profile = self.profile(cx);
                // Keeping the profile publishes nothing, so it needs no
                // validation beyond a name.
                let keeps = found.matches(&profile);
                let ready = if keeps {
                    !profile.name.is_empty()
                } else {
                    account::validate(&profile).is_ok()
                };
                let about_len = self.about.read(cx).value().chars().count();
                let field =
                    |label: &'static str, input: AnyElement, hint: Option<String>| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(0.5))
                            .child(div().text_label_sm().child(label))
                            .child(input)
                            .children(hint.map(|hint| {
                                div().text_body_sm().text_color(c.text_subtle).child(hint)
                            }))
                    };
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .child(relay_note(origin))
                    .child(div().text_body_sm().child(if found.profile.is_some() {
                        "Your existing community profile is loaded. Keep it or update it here."
                    } else {
                        "Start with your local profile, or choose how you appear in this \
                         community."
                    }))
                    .child(field(
                        "Display name",
                        TextField::new(&self.display_name)
                            .surface(Surface::Floating)
                            .disabled(self.busy)
                            .into_any_element(),
                        None,
                    ))
                    .child(field(
                        "Profile description (optional)",
                        TextArea::new(&self.about)
                            .rows(3)
                            .surface(Surface::Floating)
                            .invalid(about_len > account::ABOUT_LIMIT)
                            .disabled(self.busy)
                            .into_any_element(),
                        Some(format!(
                            "{about_len} of {} characters",
                            account::ABOUT_LIMIT
                        )),
                    ))
                    .children(self.error.clone().map(|message| error(message, cx)));
                let label = if self.busy {
                    "Working…"
                } else if keeps {
                    "Open community"
                } else {
                    "Publish profile & open"
                };
                dialog = dialog
                    .child(body)
                    .footer_leading(back("profile-back", cx))
                    .footer(
                        Button::new("profile-publish", label)
                            .prominent()
                            .disabled(self.busy || !ready)
                            .on_click(cx.listener(|this, _, window, cx| this.publish(window, cx))),
                    );
            }
        }
        dialog
    }
}
