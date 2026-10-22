//! The Enroll profile form in Settings: the developer profile the Sandbox
//! manager stores, sent as the signed-in owner. Its tokens are masked,
//! cleared once sent, and never saved.

use super::enroll::{Field, Outcome, Values};
use crate::agent_plugins::tabs::field;
use crate::community::{AppState, IdentityState};
use buzz_identity::Identity;
use buzz_relay_client::{RelayOrigin, Session};
use buzz_ui::{Colors, Surface, TextArea, TextField, TypeStyled, tokens::space};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::*;
use std::sync::Arc;

pub struct EnrollForm {
    /// Every field but the via profile.
    inputs: Vec<(Field, Entity<InputState>)>,
    via: Entity<TextareaState>,
    pub sending: bool,
    /// How the last enrollment ended.
    pub status: Option<Result<Outcome, String>>,
}

impl EnrollForm {
    /// The Git email starts as this device's `git config --global user.email`.
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let email = local_git_email();
        let inputs = Field::ALL
            .into_iter()
            .filter(|field| *field != Field::ViaProfile)
            .map(|field| {
                let state = cx.new(|cx| {
                    let state = InputState::new(window, cx).masked(field.secret());
                    match field {
                        Field::GitEmail => state
                            .placeholder("A verified email on your GitHub account")
                            .default_value(email.clone()),
                        Field::CodexAccessToken => state.placeholder("Optional · at-…"),
                        Field::ClaudeOauthToken => state.placeholder("Optional · sk-ant-oat01-…"),
                        Field::OpenaiApiKey | Field::AnthropicApiKey | Field::OpenrouterApiKey => {
                            state.placeholder("Optional")
                        }
                        _ => state,
                    }
                });
                (field, state)
            })
            .collect();
        let via = cx.new(|cx| TextareaState::new(window, cx).placeholder("via profile TOML"));
        Self {
            inputs,
            via,
            sending: false,
            status: None,
        }
    }

    /// The inputs, for change notifications.
    pub fn inputs(&self) -> impl Iterator<Item = &Entity<InputState>> {
        self.inputs.iter().map(|(_, input)| input)
    }

    fn values(&self, cx: &App) -> Values {
        let mut values = Values::default();
        for (field, input) in &self.inputs {
            values.set(*field, input.read(cx).value().to_string());
        }
        values.set(Field::ViaProfile, self.via.read(cx).value().to_string());
        values
    }

    fn write(&self, values: &Values, window: &mut Window, cx: &mut App) {
        for (field, input) in &self.inputs {
            let text = values.get(*field).to_owned();
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        let text = values.get(Field::ViaProfile).to_owned();
        self.via
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// Takes the form's values to send, leaving its tokens cleared.
    pub fn submit(&self, window: &mut Window, cx: &mut App) -> Values {
        let mut values = self.values(cx);
        let sent = values.submit();
        self.write(&values, window, cx);
        sent
    }

    pub fn render(&self, owner: Option<&str>, disabled: bool, c: &Colors) -> Div {
        let input = |which: Field| {
            let (_, state) = self
                .inputs
                .iter()
                .find(|(f, _)| *f == which)
                .expect("every field");
            div().flex_1().min_w(rems(14.)).child(field(
                which.label(),
                TextField::new(state)
                    .surface(Surface::Floating)
                    .disabled(disabled),
                None,
                c,
            ))
        };
        let row = |fields: &[Field]| {
            div()
                .flex()
                .flex_wrap()
                .gap(space::S4)
                .children(fields.iter().map(|field| input(*field)))
        };
        let status = self.status.as_ref().map(|status| match status {
            Ok(outcome @ Outcome::Enrolled { .. }) => div()
                .text_body_sm()
                .text_color(c.text_success)
                .child(outcome.text()),
            Ok(outcome) => div()
                .text_body_sm()
                .text_color(c.text_danger)
                .child(outcome.text()),
            Err(error) => div()
                .text_body_sm()
                .text_color(c.text_danger)
                .child(error.clone()),
        });
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .child(div().text_label_sm().child("Enroll profile"))
            .child(div().text_body_sm().text_color(c.text_subtle).child(
                "Once per developer: every one of your Remote workspace agents uses it. Enrolling \
                     creates a signing key for your agents' commits and adds it to your GitHub \
                     account. Sent encrypted to the Sandbox manager and signed as you; tokens \
                     are never saved here.",
            ))
            .child(field(
                "Buzz owner",
                div()
                    .text_body_sm()
                    .child(owner.unwrap_or("Sign in first").to_owned()),
                None,
                c,
            ))
            .child(row(&[Field::GithubToken, Field::GitEmail]))
            .child(field(
                Field::ViaProfile.label(),
                TextArea::new(&self.via)
                    .rows(6)
                    .surface(Surface::Floating)
                    .disabled(disabled),
                Some("Needs providers and services.".into()),
                c,
            ))
            .child(row(&[Field::OpToken]))
            // Subscriptions first: both can run in parallel in every Sandbox.
            .child(row(&[Field::CodexAccessToken, Field::ClaudeOauthToken]))
            .child(row(&[
                Field::OpenaiApiKey,
                Field::AnthropicApiKey,
                Field::OpenrouterApiKey,
            ]))
            .children(status)
    }
}

/// `git config --global user.email` on this device, or empty.
fn local_git_email() -> String {
    std::process::Command::new("git")
        .args(["config", "--global", "user.email"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|email| email.trim().to_owned())
        .unwrap_or_default()
}

/// The signed-in owner.
pub fn owner(cx: &App) -> Option<Arc<Identity>> {
    match &cx.global::<AppState>().identity {
        IdentityState::Ready(identity) => Some(identity.clone()),
        _ => None,
    }
}

/// The live session of the joined community at `relay`.
pub fn session(relay: &str, cx: &App) -> Option<Arc<Session>> {
    let origin = RelayOrigin::parse(relay).ok()?.http();
    cx.global::<AppState>()
        .communities
        .iter()
        .map(|community| community.read(cx))
        .find(|community| community.origin.http() == origin)
        .map(|community| community.session.clone())
}
