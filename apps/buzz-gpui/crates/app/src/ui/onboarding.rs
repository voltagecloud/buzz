//! First-run screens: set up an identity (`IdentitySetup.tsx`), then add a
//! community.

use super::add_community::AddCommunity;
use crate::community::{AppState, IdentityState, identity};
use buzz_ui::{ActiveTheme, Button, ButtonVariant, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Identity,
    Community,
}

pub struct Onboarding {
    /// The private key being imported; masked.
    nsec: Entity<InputState>,
    /// The import form is open ("Use an existing key").
    importing: bool,
    /// A create or import is saving the key.
    busy: bool,
    /// Why the last create or import failed.
    error: Option<SharedString>,
    /// The community step is the "Add a community" flow, invites included.
    community: Option<Entity<AddCommunity>>,
    _subscriptions: Vec<Subscription>,
}

impl Onboarding {
    pub fn new(step: Step, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let nsec = cx.new(|cx| InputState::new(window, cx).masked(true));
        let community =
            (step == Step::Community).then(|| cx.new(|cx| AddCommunity::new(true, window, cx)));
        let _subscriptions = vec![
            // Loading, the storage error and its retry all happen while
            // this step stays on screen.
            cx.observe_global::<AppState>(|_, cx| cx.notify()),
            cx.subscribe_in(
                &nsec,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => this.import(window, cx),
                    _ => {}
                },
            ),
        ];
        Self {
            nsec,
            importing: false,
            busy: false,
            error: None,
            community,
            _subscriptions,
        }
    }

    /// "Use this key": the field is cleared before the key is saved.
    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let supplied = self.nsec.read(cx).value().to_string();
        if self.busy || supplied.trim().is_empty() {
            return;
        }
        self.nsec
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.save(Some(supplied), cx);
    }

    /// Saves a new or imported key off the UI thread, as buzz-app's
    /// `identity_create` and `identity_import` do.
    fn save(&mut self, nsec: Option<String>, cx: &mut Context<Self>) {
        if self.busy || !matches!(cx.global::<AppState>().identity, IdentityState::Missing) {
            return;
        }
        self.busy = true;
        self.error = None;
        let saving = cx.background_spawn(async move { identity::save(nsec.as_deref()) });
        cx.spawn(async move |this, cx| {
            let result = saving.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(identity) => AppState::set_identity(identity, cx),
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn identity_step(&self, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let paragraph = |text: SharedString| div().text_body().child(text);
        // `<main className="p-8">`; the heading's base margins are 8 above
        // and 32 below, paragraphs 8 apart.
        let page = div()
            .size_full()
            .p(rems(2.))
            .flex()
            .flex_col()
            .text_color(c.text_standard)
            .child(
                div()
                    .mt(rems(0.5))
                    .mb(rems(2.))
                    .text_heading()
                    .child("Your Buzz identity"),
            );
        match &cx.global::<AppState>().identity {
            IdentityState::Loading | IdentityState::Ready(_) => {
                page.child(paragraph("Opening your saved identity…".into()))
            }
            IdentityState::Error(error) => page
                .child(div().mb(rems(0.5)).child(paragraph(error.clone().into())))
                .child(
                    div().flex().child(
                        Button::new("identity-retry", "Retry secure storage access")
                            .on_click(|_, _, cx| AppState::restore(cx)),
                    ),
                ),
            IdentityState::Missing => {
                let busy = self.busy;
                let controls = if self.importing {
                    let empty = self.nsec.read(cx).value().trim().is_empty();
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_body().child("Private key (nsec)"))
                        .child(TextField::new(&self.nsec).disabled(busy))
                        .child(
                            div().my(rems(0.5)).child(paragraph(
                                "Copy this from Identity details in your old Buzz app. Never \
                             share it with anyone."
                                    .into(),
                            )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap(rems(0.75))
                                .child(
                                    Button::new("identity-import", "Use this key")
                                        .variant(ButtonVariant::Prominent)
                                        .disabled(busy || empty)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.import(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("identity-back", "Back")
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.nsec.update(cx, |state, cx| {
                                                state.set_value("", window, cx)
                                            });
                                            this.importing = false;
                                            cx.notify();
                                        })),
                                ),
                        )
                } else {
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(rems(0.75))
                        .child(
                            Button::new("identity-existing", "Use an existing key")
                                .variant(ButtonVariant::Prominent)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.importing = true;
                                    this.nsec.update(cx, |state, cx| state.focus(window, cx));
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("identity-create", "Create a new identity")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.save(None, cx))),
                        )
                };
                page.child(paragraph(
                    "Already use Buzz? Bring your existing private key to keep the same \
                     identity."
                        .into(),
                ))
                .child(
                    div().mt(rems(0.5)).child(paragraph(
                        "Your key is stored securely on this device. No community is joined \
                     automatically."
                            .into(),
                    )),
                )
                .children(
                    self.error
                        .clone()
                        .map(|error| div().mt(rems(0.5)).child(paragraph(error))),
                )
                .child(div().mt(rems(0.5)).child(controls))
                .when(busy, |this| {
                    this.child(
                        div()
                            .mt(rems(0.5))
                            .child(paragraph("Saving your identity securely…".into())),
                    )
                })
            }
        }
    }
}

impl Render for Onboarding {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(community) = &self.community {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(community.clone())
                .into_any_element();
        }
        self.identity_step(cx).into_any_element()
    }
}
