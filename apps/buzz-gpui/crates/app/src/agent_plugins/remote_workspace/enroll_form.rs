//! The Enroll profile form in Settings: the developer profile the Sandbox
//! manager stores, sent as the signed-in owner. Its tokens are masked,
//! cleared once sent, and never saved.

use super::enroll::{self, Field, Outcome, Values};
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
    /// `developer` prefills the developer ID.
    pub fn new(developer: &str, window: &mut Window, cx: &mut App) -> Self {
        let inputs = Field::ALL
            .into_iter()
            .filter(|field| *field != Field::ViaProfile)
            .map(|field| {
                let state = cx.new(|cx| {
                    let state = InputState::new(window, cx).masked(field.secret());
                    match field {
                        Field::Developer => state.default_value(developer.to_owned()),
                        Field::AnthropicApiKey | Field::ClaudeOauthToken | Field::OpenaiApiKey => {
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

    /// Takes the profile for `owner` out of the form, leaving its tokens
    /// cleared whether or not it is valid.
    pub fn submit(
        &self,
        owner: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<serde_json::Value, String> {
        let mut values = self.values(cx);
        let sent = values.submit();
        self.write(&values, window, cx);
        enroll::profile(&sent, owner)
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
                "The Sandbox manager stores this profile for your Sandboxes. Sent encrypted \
                     to the manager and signed as you; tokens are never saved here.",
            ))
            .child(field(
                "Buzz owner",
                div()
                    .text_body_sm()
                    .child(owner.unwrap_or("Sign in first").to_owned()),
                None,
                c,
            ))
            .child(row(&[Field::Developer]))
            .child(row(&[Field::GitName, Field::GitEmail, Field::SigningKey]))
            .child(row(&[Field::GithubLogin, Field::GithubToken]))
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
