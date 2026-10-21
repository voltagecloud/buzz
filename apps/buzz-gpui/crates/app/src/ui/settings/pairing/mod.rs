//! Settings → Account → Pair mobile (buzz-app's `src/bundled/pairing`):
//! connect the Buzz phone app to this account and community. Opening the
//! section starts an attempt for the selected community and shows its QR
//! code; the phone then enters the six-digit code shown here (or, if it
//! is older, the user confirms that both codes match), and the account is
//! sent end to end encrypted. Leaving the section, or switching account or
//! community, cancels the attempt.
//!
//! Self-contained so it can move onto a Settings card registry: the page
//! only makes the view when the section opens and drops it on leaving.

mod client;
mod fixture;
mod qr;

use crate::community::{AppState, IdentityState};
use buzz_pair_mobile::Status;
use buzz_ui::{ActiveTheme, Button, ButtonShape, Icon, IconName, Spinner, TextField, TypeStyled};
pub use client::close;
use client::{AppNative, Native, PairingClient};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;

const TITLE: &str = "Pair mobile";
const INTRO: &str = "Connect the Buzz mobile app to this community. Pairing is secured with \
                     end-to-end encryption and a verification code.";
const ADDRESS: &str = "Enter your community’s https:// or wss:// address.";
const MISMATCH: &str = "Your saved Buzz account doesn’t match this account. Open the matching \
                        account before pairing.";
/// buzz-app shows this in its desktop shell with the development broker.
/// This app always signs with its own identity, so only a development
/// fixture shows it.
const UNAVAILABLE: &str = "Pairing uses this app’s native identity and is unavailable with the \
                           development broker. Restart with BUZZ_DEV_VIEWER empty to use native \
                           sign-in.";
/// `@container (min-width: 46rem)`: the QR beside the steps.
const WIDE: f32 = 46. * 16.;

/// One of the three numbered steps beside the code.
#[derive(Debug, PartialEq)]
struct Step {
    title: &'static str,
    detail: &'static str,
    complete: bool,
}

/// The steps for `state` (`steps` in `PairingSettings.tsx`).
fn steps(state: &Status) -> [Step; 3] {
    let scanned = matches!(
        state,
        Status::Code { .. } | Status::Transferring | Status::Complete | Status::Uncertain
    );
    let confirmed = matches!(state, Status::Transferring | Status::Complete);
    let done = *state == Status::Complete;
    let legacy = is_legacy(state);
    [
        Step {
            title: "Scan QR code",
            detail: "Open Buzz on your phone and choose Add community to scan the code shown here.",
            complete: scanned,
        },
        Step {
            title: if legacy {
                "Confirm mobile code"
            } else {
                "Enter code on your phone"
            },
            detail: if legacy {
                "This phone uses code comparison. Check that both devices show the same six digits."
            } else {
                "After scanning, enter the six-digit code shown on this desktop into your phone."
            },
            complete: confirmed,
        },
        Step {
            title: if done {
                "Paired"
            } else {
                "Pair your mobile app"
            },
            detail: if done {
                "Your account was sent. Check that it’s signed in on your phone."
            } else {
                "Your mobile app will connect after you verify the code."
            },
            complete: done,
        },
    ]
}

/// An older phone compares codes instead of entering one.
fn is_legacy(state: &Status) -> bool {
    matches!(
        state,
        Status::Code {
            code_entry: false,
            ..
        }
    )
}

/// While an attempt is running, the card hides its own actions.
fn busy(state: &Status) -> bool {
    matches!(
        state,
        Status::Connecting
            | Status::Qr { .. }
            | Status::Code { .. }
            | Status::Transferring
            | Status::Cancelling
    )
}

/// A community is a secure origin (`relayOrigin`): `wss://` or
/// `https://` with only a host, normalized to its `https://` origin.
fn relay_origin(input: &str) -> Option<String> {
    let value = input.trim();
    let (scheme, rest) = value.split_once("://")?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if value.len() > 2048
        || !matches!(scheme.to_ascii_lowercase().as_str(), "wss" | "https")
        || authority.is_empty()
        || authority
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '\\' | '@') || c.is_whitespace())
    {
        return None;
    }
    let mut url = url::Url::parse(value).ok()?;
    url.set_scheme("https").ok()?;
    // DNS treats the final dot as the same host.
    if let Some(host) = url.host_str().filter(|host| host.ends_with('.')) {
        let host = host.trim_end_matches('.').to_owned();
        url.set_host(Some(&host)).ok()?;
    }
    Some(url.origin().ascii_serialization())
}

/// Who is pairing where: the live account and the selected community's
/// address (`client.viewer`, `communityDestination(client.selected).url`).
#[derive(Clone, Debug, Default, PartialEq)]
struct Scope {
    viewer: Option<String>,
    selected: Option<String>,
}

impl Scope {
    fn current(cx: &App) -> Self {
        let viewer = match &cx.global::<AppState>().identity {
            IdentityState::Ready(identity) => Some(identity.pubkey_hex()),
            _ => None,
        };
        let selected = AppState::current(cx).map(|community| community.read(cx).origin.http());
        Self { viewer, selected }
    }
}

/// Registers the "Pair mobile" card in the Account group
/// (`pairing/index.tsx`).
pub fn init(cx: &mut App) {
    super::cards::register(
        super::cards::SettingsCard {
            key: "buzz.pairing/mobile",
            title: "Pair mobile",
            // A contributed card without its own icon.
            icon: Some(IconName::ChatCircle),
            placement: super::cards::CardPlacement::Group("Account"),
            visibility: None,
            build: super::cards::build(|window, cx| {
                cx.new(|cx| PairingSettings::new(window, cx)).into()
            }),
        },
        cx,
    );
}

pub struct PairingSettings {
    client: Entity<PairingClient>,
    available: bool,
    scope: Scope,
    /// The account the user chose to pair.
    account: Option<String>,
    /// The address typed when no community is selected, once started.
    manual_origin: Option<String>,
    error: Option<SharedString>,
    /// The community address: the selected community's, or typed.
    relay: String,
    relay_input: Entity<InputState>,
    reveal: qr::Reveal,
    /// The pairing code last copied: "Copied" shows while it is the one on
    /// screen.
    copied: Option<String>,
    /// The card's width at the last layout, for its container query.
    width: Rc<Cell<Pixels>>,
    _subscriptions: Vec<Subscription>,
}

impl PairingSettings {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let native: Rc<dyn Native> = match fixture::Fixture::from_env() {
            Some(fixture) => Rc::new(fixture),
            None => Rc::new(AppNative::new(cx)),
        };
        let available = native.available();
        let client = cx.new(|cx| PairingClient::new(native, cx));
        let relay_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://your-community.example"));
        let subscriptions = vec![
            cx.observe(&client, |this, _, cx| {
                this.autostart(cx);
                cx.notify();
            }),
            cx.observe_global_in::<AppState>(window, |this, window, cx| {
                let scope = Scope::current(cx);
                if scope != this.scope {
                    // Another account or community invalidates the attempt.
                    this.client
                        .update(cx, |client, cx| client.cancel(true, cx).detach());
                    this.rescope(scope, window, cx);
                }
            }),
            cx.subscribe_in(
                &relay_input,
                window,
                |this, input, event, _, cx| match event {
                    InputEvent::Change => this.relay = input.read(cx).value().to_string(),
                    InputEvent::Blur | InputEvent::PressEnter { .. } => {
                        if !this.relay.trim().is_empty() {
                            this.start(cx);
                        }
                    }
                    InputEvent::Focus => {}
                },
            ),
        ];
        let mut this = Self {
            client,
            available,
            scope: Scope::default(),
            account: None,
            manual_origin: None,
            error: None,
            relay: String::new(),
            relay_input,
            reveal: qr::Reveal::default(),
            copied: None,
            width: Rc::new(Cell::new(px(WIDE))),
            _subscriptions: subscriptions,
        };
        this.rescope(Scope::current(cx), window, cx);
        this
    }

    fn rescope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        self.manual_origin = None;
        self.account = scope.viewer.clone();
        self.relay = scope.selected.clone().unwrap_or_default();
        let relay = self.relay.clone();
        self.relay_input
            .update(cx, |input, cx| input.set_value(relay, window, cx));
        self.scope = scope;
        self.autostart(cx);
        cx.notify();
    }

    fn state<'a>(&self, cx: &'a App) -> &'a Status {
        self.client.read(cx).state()
    }

    /// Starts on its own while the section is open, and again when a code
    /// expires.
    fn autostart(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let Scope { viewer, selected } = &self.scope;
        if !self.available
            || (selected.is_none() && self.manual_origin.is_none())
            || viewer.as_ref().is_some_and(|viewer| *viewer != account)
            || selected
                .as_ref()
                .is_some_and(|selected| *selected != self.relay)
            || !matches!(self.state(cx), Status::Idle | Status::Expired)
        {
            return;
        }
        let origin = match selected {
            Some(selected) => Some(selected.clone()),
            None => self.manual_origin.clone(),
        };
        if let Some(origin) = origin {
            self.client
                .update(cx, |client, cx| client.start(account, origin, cx));
        }
    }

    /// Where to pair: the selected community's origin, already normalized
    /// (native refuses a development one on this machine that isn't
    /// secure), or the typed address.
    fn origin(&self) -> Option<String> {
        match &self.scope.selected {
            Some(selected) => Some(selected.clone()),
            None => relay_origin(&self.relay),
        }
    }

    /// "Try again", "Start a new pairing", "Pair another phone", and the
    /// typed address.
    fn start(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let Some(account) = self.account.clone() else {
            return;
        };
        match self.origin() {
            Some(origin) => {
                if self.scope.selected.is_none() {
                    self.manual_origin = Some(origin.clone());
                }
                self.client
                    .update(cx, |client, cx| client.start(account, origin, cx));
            }
            None => self.error = Some(ADDRESS.into()),
        }
        cx.notify();
    }

    /// "Use existing Buzz account": reads only the public key. The read is
    /// in memory here, so buzz-app's "Opening your account…" never shows.
    fn prepare(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let native = self.client.read(cx).native();
        match native.account(cx) {
            Ok(viewer)
                if self
                    .scope
                    .viewer
                    .as_ref()
                    .is_some_and(|current| *current != viewer) =>
            {
                self.error = Some(MISMATCH.into());
            }
            Ok(viewer) => self.account = Some(viewer),
            Err(error) => self.error = Some(error.into()),
        }
        self.autostart(cx);
        cx.notify();
    }

    /// What fills the square beside the steps.
    fn panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = *cx.colors();
        // In buzz-app these `<p>`s, unlike the `m-0` ones, keep an 8px
        // block margin.
        let muted = |text: &'static str| {
            div()
                .my(rems(0.5))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
                .into_any_element()
        };
        if !self.available {
            return vec![muted(UNAVAILABLE)];
        }
        let state = self.state(cx).clone();
        let done = state == Status::Complete;
        let mut children = Vec::new();
        if !busy(&state) && !done && state != Status::Uncertain {
            match &self.account {
                None => children.push(
                    button("pair-account", "Use existing Buzz account")
                        .on_click(cx.listener(|this, _, _, cx| this.prepare(cx)))
                        .into_any_element(),
                ),
                Some(_) => {
                    if self.scope.selected.is_none() {
                        children.push(
                            div()
                                .flex()
                                .flex_col()
                                .gap(rems(0.5))
                                .w_full()
                                .text_left()
                                .child(div().text_label_sm().child("Community address"))
                                .child(TextField::new(&self.relay_input))
                                .into_any_element(),
                        );
                    }
                    if matches!(state, Status::Error { .. } | Status::Cancelled) {
                        children.push(
                            button("pair-retry", "Try again")
                                .prominent()
                                .on_click(cx.listener(|this, _, _, cx| this.start(cx)))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        let progress = |text: &'static str| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(rems(0.75))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(Spinner::new().size(24.))
                .child(text)
                .into_any_element()
        };
        match &state {
            Status::Connecting => children.push(progress("Creating pairing code…")),
            Status::Qr { matrix, .. } => {
                let elapsed = self.reveal.elapsed(matrix, window, cx);
                children.push(qr::qr_code(matrix.clone(), elapsed).into_any_element());
            }
            Status::Code { code, .. } => children.push(self.code(code, is_legacy(&state), cx)),
            Status::Cancelling => children.push(muted("Cancelling pairing…")),
            Status::Cancelled => children.push(muted("Pairing was canceled.")),
            Status::Transferring => children.push(progress("Finishing pairing on your phone…")),
            Status::Uncertain => children.push(
                outcome()
                    .child(div().text_label().child("Check your phone"))
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child("The account was sent, but your phone hasn’t confirmed. It may already be paired."),
                    )
                    .child(
                        button("pair-new", "Start a new pairing")
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                    )
                    .into_any_element(),
            ),
            Status::Complete => children.push(
                outcome()
                    .child(
                        Icon::new(IconName::Check)
                            .size(40.)
                            .color(c.text_success),
                    )
                    .child(div().text_label().child("Phone paired"))
                    .child(
                        button("pair-another", "Pair another phone")
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                    )
                    .into_any_element(),
            ),
            _ => {}
        }
        let message = self.error.clone().or(match &state {
            Status::Error { message } => Some(message.clone().into()),
            _ => None,
        });
        if let Some(message) = message {
            children.push(
                div()
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child(message)
                    .into_any_element(),
            );
        }
        children
    }

    /// "Copy pairing code" below the QR (#1003). Its slot is kept in
    /// every phase so the steps never shift.
    fn copy_slot(&self, state: &Status, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.available {
            return None;
        }
        let button = match state {
            Status::Qr { uri, .. } => {
                let copied = self.copied.as_deref() == Some(uri.as_str());
                let uri = uri.clone();
                Some(
                    button(
                        "pair-copy",
                        if copied {
                            "Copied"
                        } else {
                            "Copy pairing code"
                        },
                    )
                    .full_width()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(uri.clone()));
                        this.copied = Some(uri.clone());
                        cx.notify();
                    })),
                )
            }
            _ => None,
        };
        Some(
            div()
                .flex()
                .flex_col()
                .min_h(rems(2.5))
                .children(button)
                .into_any_element(),
        )
    }

    /// The code, and what to do with it.
    fn code(&self, code: &str, legacy: bool, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        // `001 234`, with `tracking-widest` (0.1em) between the digits.
        let shown = format!(
            "{} {}",
            &code[..code.len().min(3)],
            &code[code.len().min(3)..]
        );
        let digits = div()
            .flex()
            .justify_center()
            .gap(rems(0.2))
            .font_family(buzz_ui::tokens::font::MONO)
            .text_size(rems(2.))
            .line_height(rems(2.2))
            .font_weight(FontWeight::SEMIBOLD)
            .children(shown.chars().map(|digit| div().child(digit.to_string())));
        let client = self.client.clone();
        div()
            .flex()
            .flex_col()
            .justify_between()
            .gap(rems(1.5))
            .w_full()
            .min_h(rems(16.))
            .child(div().text_label().child(if legacy {
                "Check the code on your phone"
            } else {
                "Enter this code on your phone"
            }))
            .child(
                div()
                    .w_full()
                    .rounded(buzz_ui::tokens::radius::CONTROL)
                    .border_1()
                    .border_color(c.border_standard)
                    .bg(c.surface_panel)
                    .p(rems(1.))
                    .child(digits),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .map(|this| {
                        if legacy {
                            let client = client.clone();
                            this.child(
                                button("pair-match", "Codes match")
                                    .prominent()
                                    .full_width()
                                    .on_click(move |_, _, cx| {
                                        client.update(cx, |client, cx| client.decide(true, cx))
                                    }),
                            )
                        } else {
                            this.child(div().text_body_sm().text_color(c.text_subtle).child(
                                "Pairing continues when you enter all six digits on your phone.",
                            ))
                        }
                    })
                    .child(button("pair-cancel", "Cancel").full_width().on_click(
                        move |_, _, cx| {
                            client.update(cx, |client, cx| {
                                if legacy {
                                    client.decide(false, cx)
                                } else {
                                    client.cancel(false, cx).detach()
                                }
                            })
                        },
                    )),
            )
            .into_any_element()
    }
}

/// Settings buttons use 12px corners instead of pills.
fn button(id: &'static str, label: &'static str) -> Button {
    Button::new(id, label).shape(ButtonShape::Control)
}

/// "Check your phone" and "Phone paired".
fn outcome() -> Div {
    div().flex().flex_col().items_center().gap(rems(0.75))
}

impl Render for PairingSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let wide = self.width.get() >= px(WIDE);
        let panel = self.panel(window, cx);
        let state = self.state(cx).clone();
        let copy = self.copy_slot(&state, cx);
        let width = self.width.clone();
        let steps = steps(&state).into_iter().enumerate().map(|(index, step)| {
            div()
                .flex()
                .items_start()
                .gap(rems(1.))
                .min_w_0()
                .min_h(rems(6.))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(rems(3.))
                        .rounded_full()
                        .text_label()
                        .map(|this| {
                            if step.complete {
                                this.bg(c.affordance_success)
                                    .text_color(c.text_success)
                                    .child(Icon::new(IconName::Check).size(24.))
                            } else {
                                this.bg(c.affordance_subtle)
                                    .text_color(c.text_standard)
                                    .child((index + 1).to_string())
                            }
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.25))
                        .min_w_0()
                        .child(div().text_label().child(step.title))
                        .child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child(step.detail),
                        ),
                )
        });
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .child(div().mb(rems(0.5)).text_label().child(TITLE))
            .child(
                div()
                    .mb(rems(1.5))
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(INTRO),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .child(
                        // The container query: re-lay out when the card
                        // crosses 46rem.
                        canvas(
                            move |bounds, window, _| {
                                let was = width.replace(bounds.size.width) >= px(WIDE);
                                if was != (bounds.size.width >= px(WIDE)) {
                                    window.refresh();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(rems(1.5))
                            .when(wide, |this| this.flex_row().p(rems(3.)))
                            .when(!wide, |this| this.flex_col().p(rems(1.5)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_none()
                                    .gap(rems(0.75))
                                    .w(rems(16.))
                                    .max_w_full()
                                    .when(wide, |this| this.self_start())
                                    .when(!wide, |this| this.self_center())
                                    .child(
                                        div()
                                            .id("pairing-panel")
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .justify_center()
                                            .gap(rems(1.))
                                            .h(rems(16.))
                                            .w_full()
                                            .overflow_y_scroll()
                                            .text_center()
                                            .children(panel),
                                    )
                                    .children(copy),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .self_center()
                                    .gap(rems(0.75))
                                    .w_full()
                                    .min_w_0()
                                    .children(steps),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn steps_follow_the_attempt() {
        let complete = |state: &Status| steps(state).map(|step| step.complete);
        assert_eq!(complete(&Status::Connecting), [false, false, false]);
        let code = Status::Code {
            code: "001234".into(),
            code_entry: true,
        };
        assert_eq!(complete(&code), [true, false, false]);
        assert_eq!(steps(&code)[1].title, "Enter code on your phone");
        assert_eq!(complete(&Status::Transferring), [true, true, false]);
        assert_eq!(complete(&Status::Uncertain), [true, false, false]);
        let done = steps(&Status::Complete);
        assert_eq!(done.each_ref().map(|step| step.complete), [true; 3]);
        assert_eq!(done[2].title, "Paired");
        assert_eq!(
            done[2].detail,
            "Your account was sent. Check that it’s signed in on your phone."
        );
        let legacy = steps(&Status::Code {
            code: "654321".into(),
            code_entry: false,
        });
        assert_eq!(legacy[1].title, "Confirm mobile code");
        assert_eq!(
            legacy[1].detail,
            "This phone uses code comparison. Check that both devices show the same six digits."
        );
    }

    #[test]
    fn only_running_attempts_hide_the_actions() {
        assert!(busy(&Status::Connecting));
        assert!(busy(&Status::Cancelling));
        for idle in [
            Status::Idle,
            Status::Expired,
            Status::Cancelled,
            Status::Uncertain,
            Status::Complete,
            Status::Error {
                message: String::new(),
            },
        ] {
            assert!(!busy(&idle), "{idle:?}");
        }
    }

    #[test]
    fn communities_are_secure_origins() {
        assert_eq!(
            relay_origin(" wss://relay.example.com/ ").as_deref(),
            Some("https://relay.example.com")
        );
        assert_eq!(
            relay_origin("HTTPS://Relay.Example.com.:8443").as_deref(),
            Some("https://relay.example.com:8443")
        );
        for refused in [
            "",
            "relay.example.com",
            "ws://127.0.0.1:53000",
            "http://relay.example.com",
            "https://relay.example.com/path",
            "https://user@relay.example.com",
            "https://relay.example.com?x=1",
            "https://relay.example.com#x",
        ] {
            assert_eq!(relay_origin(refused), None, "{refused}");
        }
    }
}
