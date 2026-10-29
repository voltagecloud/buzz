//! The Enroll profile form in Settings: the developer profile the Sandbox
//! manager stores, sent as the signed-in owner. It shows what is on file,
//! and once a profile is, sends only what changes. Its tokens are masked,
//! cleared once sent, and never saved.

use super::codex_login::PendingLogin;
use super::enroll::{Field, OnFile, Outcome, Values};
use super::settings::SettingsTab;
use super::signing::Registration;
use crate::agent_plugins::tabs::field;
use crate::community::{AppState, IdentityState};
use buzz_identity::Identity;
use buzz_relay_client::{RelayOrigin, Session};
use buzz_ui::{
    Button, ButtonSize, Checkbox, Colors, Surface, TextArea, TextField, TypeStyled, tokens::space,
};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

pub struct EnrollForm {
    pub codex_login: Option<PendingLogin>,
    pub logging_in: bool,
    pub login_generation: u64,
    pub login_abort: Option<tokio::task::AbortHandle>,
    pub login_status: Option<String>,
    /// Every field but the via profile.
    inputs: Vec<(Field, Entity<InputState>)>,
    via: Entity<TextareaState>,
    pub sending: bool,
    /// How the last enrollment ended.
    pub status: Option<Result<Outcome, String>>,
    /// How the last enrollment's signing key reached GitHub.
    pub registration: Option<Registration>,
    /// What the manager holds.
    pub on_file: OnFile,
    /// Replace the stored signing key instead of keeping it.
    pub replace_key: bool,
    /// Stored model credentials to remove, by models key.
    pub removed: Vec<&'static str>,
}

/// The placeholder for `field`, by whether a value is on file.
fn placeholder(field: Field, held: bool) -> &'static str {
    if held && field.secret() || held && field == Field::ViaProfile {
        return "On file · leave blank to keep";
    }
    match field {
        Field::GitEmail => "A verified email on your GitHub account",
        Field::ViaProfile => "via profile TOML",
        Field::CodexAccessToken => "Optional · at-…",
        Field::ClaudeOauthToken => "Optional · sk-ant-oat01-…",
        Field::OpenaiApiKey | Field::AnthropicApiKey | Field::OpenrouterApiKey => "Optional",
        _ => "",
    }
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
                    let state = InputState::new(window, cx)
                        .masked(field.secret())
                        .placeholder(placeholder(field, false));
                    if field == Field::GitEmail {
                        state.default_value(email.clone())
                    } else {
                        state
                    }
                });
                (field, state)
            })
            .collect();
        let via = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(placeholder(Field::ViaProfile, false))
        });
        Self {
            codex_login: None,
            logging_in: false,
            login_generation: 0,
            login_abort: None,
            login_status: None,
            inputs,
            via,
            sending: false,
            status: None,
            registration: None,
            on_file: OnFile::Loading,
            replace_key: false,
            removed: Vec::new(),
        }
    }

    /// Shows what is on file: on-file tokens keep their stored value when
    /// left blank, and the Git email starts as the stored one.
    pub fn show_on_file(&mut self, on_file: OnFile, window: &mut Window, cx: &mut App) {
        let summary = on_file.summary().cloned().unwrap_or_default();
        let stored = on_file.summary().is_some();
        for (field, input) in &self.inputs {
            let held = stored && summary.holds(*field);
            input.update(cx, |state, cx| {
                state.set_placeholder(placeholder(*field, held), window, cx);
                if *field == Field::GitEmail && held {
                    state.set_value(summary.git.email.clone(), window, cx);
                }
            });
        }
        let held = stored && summary.holds(Field::ViaProfile);
        self.via.update(cx, |state, cx| {
            state.set_placeholder(placeholder(Field::ViaProfile, held), window, cx)
        });
        self.removed.retain(|key| {
            summary.models.iter().any(|stored| stored == key)
                || (*key == "codex_login" && summary.codex_login)
        });
        if !summary.git.signing_key {
            self.replace_key = false;
        }
        self.on_file = on_file;
    }

    /// Marks the stored model credential `key` for removal, or keeps it.
    pub fn toggle_removed(&mut self, key: &'static str) {
        if let Some(at) = self.removed.iter().position(|removed| *removed == key) {
            self.removed.remove(at);
        } else {
            self.removed.push(key);
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

    pub fn render(
        &self,
        owner: Option<&str>,
        disabled: bool,
        c: &Colors,
        cx: &Context<SettingsTab>,
    ) -> Div {
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
                     creates a signing key for your agents' commits; the GitHub CLI (gh) adds it \
                     to your GitHub account, or the app shows you how. With a profile on file, \
                     fill in only what changes: a blank field keeps what is stored. Sent \
                     encrypted to the Sandbox manager and signed as you; tokens are never saved \
                     here.",
            ))
            .child(div().text_body_sm().child(self.on_file.text()))
            .children(self.on_file_controls(disabled, c, cx))
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
            .child(
                div().flex().gap(space::S2)
                    .child(Button::new("remote-workspace-chatgpt-login", "Sign in with ChatGPT")
                        .size(ButtonSize::Sm).outline().disabled(disabled || self.logging_in)
                        .on_click(cx.listener(|this, _, window, cx| this.sign_in_chatgpt(window, cx))))
                    .when(self.codex_login.is_some(), |row| row.child(
                        Button::new("remote-workspace-discard-chatgpt-login", "Discard sign-in")
                            .size(ButtonSize::Sm).outline().disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.enroll.codex_login = None;
                                this.enroll.login_status = None;
                                cx.notify();
                            }))
                    ))
                    .when(self.logging_in, |row| row.child(
                        Button::new("remote-workspace-cancel-chatgpt-login", "Cancel sign-in")
                            .size(ButtonSize::Sm).outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.enroll.login_generation = this.enroll.login_generation.wrapping_add(1);
                                if let Some(abort) = this.enroll.login_abort.take() { abort.abort(); }
                                this.enroll.logging_in = false;
                                this.enroll.login_status = Some("ChatGPT sign-in cancelled.".into());
                                cx.notify();
                            }))
                    ))
            )
            .children(self.login_status.as_ref().map(|status| div().text_body_sm().child(status.clone())))
            .child(div().text_body_sm().text_color(c.text_subtle).child(
                "Sign in once for all remote Codex agents. Enrolling this login replaces an OpenAI API key or Codex access token."
            ))
            // Subscriptions first: both can run in parallel in every Sandbox.
            .child(row(&[Field::CodexAccessToken, Field::ClaudeOauthToken]))
            .child(row(&[
                Field::OpenaiApiKey,
                Field::AnthropicApiKey,
                Field::OpenrouterApiKey,
            ]))
            .children(status)
            .children(
                self.registration
                    .as_ref()
                    .map(|registration| registration_notice(registration, c)),
            )
    }
}

impl EnrollForm {
    /// Remove or keep each stored model credential, and replace the stored
    /// signing key.
    fn on_file_controls(
        &self,
        disabled: bool,
        c: &Colors,
        cx: &Context<SettingsTab>,
    ) -> Option<Div> {
        let summary = self.on_file.summary()?;
        let removals = summary.model_fields().into_iter().filter_map(|field| {
            let key = field.model()?;
            let removing = self.removed.contains(&key);
            let label = if removing {
                format!("Keep {}", field.label())
            } else {
                format!("Remove {}", field.label())
            };
            Some(
                Button::new(
                    SharedString::from(format!("remote-workspace-remove-{key}")),
                    label,
                )
                .size(ButtonSize::Sm)
                .outline()
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.enroll.toggle_removed(key);
                    cx.notify();
                })),
            )
        });
        let removing = (!self.removed.is_empty()).then(|| {
            let names: Vec<_> = self
                .removed
                .iter()
                .filter_map(|key| Field::of_model(key))
                .map(Field::label)
                .collect();
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(format!("Enrolling removes: {}.", names.join(", ")))
        });
        let replace = summary.git.signing_key.then(|| {
            let tab = cx.entity().downgrade();
            Checkbox::new("remote-workspace-replace-signing-key", self.replace_key)
                .label("Replace the signing key")
                .description("Creates a new key for your agents' commits and adds it to GitHub.")
                .surface(Surface::Floating)
                .disabled(disabled)
                .on_change(move |checked, _, cx| {
                    let _ = tab.update(cx, |this, cx| {
                        this.enroll.replace_key = checked;
                        cx.notify();
                    });
                })
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .child(div().flex().flex_wrap().gap(space::S2).children(removals))
                .when(summary.codex_login, |view| {
                    view.child(
                        Button::new(
                            "remote-workspace-remove-chatgpt-login",
                            if self.removed.contains(&"codex_login") {
                                "Keep ChatGPT login"
                            } else {
                                "Disconnect ChatGPT login"
                            },
                        )
                        .size(ButtonSize::Sm)
                        .outline()
                        .disabled(disabled)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.enroll.toggle_removed("codex_login");
                            if this.enroll.removed.contains(&"codex_login") {
                                this.enroll.codex_login = None;
                                this.enroll.login_status = None;
                            }
                            cx.notify();
                        })),
                    )
                })
                .children(removing)
                .children(replace),
        )
    }
}

/// Where the agents' signing key went: added with `gh`, or the steps for the
/// developer to add it, so their agents' commits show as Verified.
fn registration_notice(registration: &Registration, c: &Colors) -> Div {
    match registration {
        Registration::Added => div().text_body_sm().text_color(c.text_success).child(
            "Added your agents' signing key to your GitHub account (Settings → SSH and GPG keys).",
        ),
        Registration::Manual { public, reason } => {
            let copied = public.clone();
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .child(div().text_label_sm().child("Add your agents' signing key to GitHub"))
                .child(div().text_body_sm().text_color(c.text_subtle).child(format!(
                    "Until you add it, your agents' commits are signed but show as Unverified. \
                     The app could not add it for you: {reason}."
                )))
                .child(div().text_body_sm().child(
                    "1. On GitHub, open Settings → SSH and GPG keys and press New SSH key. \
                     2. Set Key type to Signing Key. 3. Paste the key below and press Add SSH key.",
                ))
                .child(div().text_body_sm().font_family("monospace").child(public.clone()))
                .child(
                    div().flex().child(
                        Button::new("remote-workspace-copy-signing-key", "Copy key")
                            .size(ButtonSize::Sm)
                            .outline()
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                            }),
                    ),
                )
        }
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
