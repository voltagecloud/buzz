//! Steps 2 to 4: the credentials the profile carries, collected in memory
//! only. With a profile on file, each step keeps what is stored unless a
//! new value is given, as the Buzz app's enroll form does.

use serde_json::{Map, Value};

use super::io::{redacted, Prompter};
use super::manager::ControlTransport;
use super::profile::{self, Field, GitHubIdentity, Values};
use super::relay::Relay;
use super::steps_identity::OnFile;
use super::tools::{SigningKey, Tools};
use super::Wizard;
use crate::error::CliError;

/// The shared Voltage via profile (voltage/remote-agents.md, "Shared via
/// profile"): only `op://` references, no secret values.
pub const SHARED_VIA_PROFILE: &str = include_str!("via-profile.toml");

/// What steps 2 to 4 collected. Wiped on drop.
#[derive(Default)]
pub struct Credentials {
    /// The values to send, by field.
    pub values: Values,
    /// A new GitHub token's account; `None` keeps the stored token.
    pub github: Option<GitHubIdentity>,
    /// A new agent signing key; `None` keeps the stored one.
    pub signing_key: Option<SigningKey>,
    /// The GitHub login the signing key belongs on.
    pub login: String,
}

impl Credentials {
    /// Whether anything would change the stored profile.
    pub fn changes(&self) -> bool {
        self.github.is_some()
            || self.signing_key.is_some()
            || Field::ALL
                .into_iter()
                .any(|field| self.values.filled(field))
    }
}

impl<P: Prompter, R: Relay, T: ControlTransport, X: Tools> Wizard<P, R, T, X> {
    /// Step 2. The GitHub token (from `BUZZ_ONBOARD_GITHUB_TOKEN`, `gh auth
    /// token`, or a hidden prompt) and its account, the Git email, and a new
    /// agent-only Ed25519 signing key from `ssh-keygen` when none is stored.
    /// The key's public half goes to GitHub only after the manager stores
    /// the profile ([`Wizard::enroll`]), as in the Buzz app.
    pub async fn github(
        &mut self,
        on_file: &OnFile,
        credentials: &mut Credentials,
    ) -> Result<(), CliError> {
        let summary = on_file.summary.as_ref();
        let token_on_file = summary.is_some_and(|summary| summary.github.token);
        if token_on_file && !self.args.reenroll && self.env.github_token.is_none() {
            credentials.login = summary.map(|s| s.github.login.clone()).unwrap_or_default();
            let login = credentials.login.clone();
            self.say(&format!("  GitHub token on file for @{login}."));
        } else {
            let token = self.github_token(token_on_file).await?;
            if token.is_empty() {
                credentials.login = summary.map(|s| s.github.login.clone()).unwrap_or_default();
            } else {
                let identity = self
                    .tools
                    .github_identity(&token)
                    .await
                    .map_err(CliError::Auth)?;
                self.say(&format!("  GitHub token for @{}.", identity.login));
                credentials.login = identity.login.clone();
                credentials
                    .values
                    .set(Field::GithubToken, token.to_string());
                credentials.github = Some(identity);
            }
        }
        let email_on_file = summary.is_some_and(|summary| !summary.git.email.is_empty());
        if self.args.git_email.is_some() || !email_on_file {
            let email = match self.args.git_email.clone() {
                Some(email) => email,
                None => {
                    let local = self.tools.git_email().await;
                    let default = Some(local.as_str()).filter(|email| !email.is_empty());
                    self.prompter.ask(
                        "  Git email (a verified email on your GitHub account)",
                        default,
                        "pass --git-email",
                    )?
                }
            };
            if email.trim().is_empty() {
                return Err(CliError::Usage("A Git email is required.".into()));
            }
            credentials
                .values
                .set(Field::GitEmail, email.trim().to_owned());
        }
        let key_on_file = summary.is_some_and(|summary| summary.git.signing_key);
        if !key_on_file || self.args.replace_signing_key {
            if credentials.login.is_empty() {
                return Err(CliError::Usage("A GitHub token is required.".into()));
            }
            let comment = format!("buzz agents {}", credentials.login);
            let key = self
                .tools
                .ssh_keygen(&comment)
                .await
                .map_err(CliError::Other)?;
            self.say(
                "  Created an agent-only Ed25519 signing key. It goes to GitHub after you enroll.",
            );
            credentials.signing_key = Some(key);
        }
        Ok(())
    }

    async fn github_token(
        &mut self,
        on_file: bool,
    ) -> Result<zeroize::Zeroizing<String>, CliError> {
        if let Some(token) = self.env.github_token.take() {
            return Ok(token);
        }
        if !on_file {
            if let Some(token) = self.tools.gh_auth_token().await {
                let question = format!(
                    "  Use the GitHub CLI's token ({})? A fine-grained token scoped to voltagecloud is safer",
                    redacted(&token)
                );
                if self.prompter.confirm(&question, true)? {
                    return Ok(token);
                }
            }
        }
        let question = if on_file {
            "  GitHub token (blank keeps the one on file)"
        } else {
            "  GitHub fine-grained token"
        };
        let token = self.prompter.secret(
            question,
            "set BUZZ_ONBOARD_GITHUB_TOKEN or sign in with gh auth login",
        )?;
        if token.is_empty() && !on_file {
            return Err(CliError::Usage("A GitHub token is required.".into()));
        }
        Ok(token)
    }

    /// Step 3. Claude subscription token (`claude setup-token`), Codex access
    /// token and OpenRouter key from `BUZZ_ONBOARD_*` or hidden prompts;
    /// OpenAI and Anthropic API keys from the environment only. Checked with
    /// the profile's rules, including that each pair is mutually exclusive.
    pub async fn models(
        &mut self,
        on_file: &OnFile,
        credentials: &mut Credentials,
    ) -> Result<(), CliError> {
        let summary = on_file.summary.clone().unwrap_or_default();
        let from_env = [
            (Field::ClaudeOauthToken, self.env.claude_token.take()),
            (Field::CodexAccessToken, self.env.codex_token.take()),
            (Field::OpenrouterApiKey, self.env.openrouter_key.take()),
            (Field::OpenaiApiKey, self.env.openai_api_key.take()),
            (Field::AnthropicApiKey, self.env.anthropic_api_key.take()),
        ];
        for (field, value) in from_env {
            if let Some(value) = value {
                credentials.values.set(field, value.to_string());
            }
        }
        if self.prompter.interactive() {
            let asked = [
                (Field::ClaudeOauthToken, "sk-ant-oat01-…"),
                (Field::CodexAccessToken, "at-…"),
                (Field::OpenrouterApiKey, "sk-or-…"),
            ];
            for (field, shape) in asked {
                if credentials.values.filled(field) {
                    continue;
                }
                let held = summary.holds(field);
                if field == Field::ClaudeOauthToken
                    && !held
                    && self.prompter.confirm(
                        "  Run claude setup-token now to create a Claude subscription token?",
                        false,
                    )?
                    && !self.tools.claude_setup_token().await
                {
                    self.say(
                        "  claude setup-token did not finish. Paste a token, or leave it blank.",
                    );
                }
                let keep = if held {
                    "blank keeps the one on file"
                } else {
                    "blank to skip"
                };
                let question = format!("  {} ({shape}, {keep})", field.label());
                let value = self.prompter.secret(&question, "")?;
                credentials.values.set(field, value.to_string());
            }
        }
        let models: Map<String, Value> = Field::ALL
            .into_iter()
            .filter(|field| credentials.values.filled(*field))
            .filter_map(|field| {
                Some((
                    field.model()?.to_owned(),
                    credentials.values.get(field).trim().into(),
                ))
            })
            .collect();
        profile::models(&Value::Object(models.clone())).map_err(CliError::Usage)?;
        let entered: Vec<_> = Field::ALL
            .into_iter()
            .filter(|field| field.model().is_some() && credentials.values.filled(*field))
            .map(Field::label)
            .collect();
        if !entered.is_empty() {
            self.say(&format!("  Will send: {}.", entered.join(", ")));
        }
        self.available_models = summary.models.clone();
        self.available_models.extend(models.keys().cloned());
        if summary.codex_login {
            self.available_models.push("codex_login".into());
        }
        if models.is_empty() && summary.models.is_empty() && !summary.codex_login {
            self.actions.push(
                "Add a model credential (BUZZ_ONBOARD_CLAUDE_TOKEN, BUZZ_ONBOARD_CODEX_TOKEN or BUZZ_ONBOARD_OPENROUTER_KEY) and run buzz onboard --reenroll: your agents cannot run a model yet.".into(),
            );
        }
        Ok(())
    }

    /// Step 4. The 1Password service account token (`ops_…`) that reads the
    /// Buzz Agents Dev vault, and the via profile (the shared one unless
    /// `--via-profile`). Both are required for a first enrollment.
    pub fn op_token(
        &mut self,
        on_file: &OnFile,
        credentials: &mut Credentials,
    ) -> Result<(), CliError> {
        let summary = on_file.summary.clone().unwrap_or_default();
        let held = summary.holds(Field::OpToken);
        let token = match self.env.op_token.take() {
            Some(token) => token,
            None if held && !self.args.reenroll => {
                self.say("  1Password token on file.");
                Default::default()
            }
            None => {
                let question = if held {
                    "  1Password service account token (ops_…, blank keeps the one on file)"
                } else {
                    "  1Password service account token (ops_…)"
                };
                self.prompter
                    .secret(question, "set BUZZ_ONBOARD_OP_TOKEN")?
            }
        };
        if token.is_empty() && !held {
            return Err(CliError::Usage(
                "A 1Password service account token is required (see voltage/remote-agents.md, 1Password service account).".into(),
            ));
        }
        credentials.values.set(Field::OpToken, token.to_string());
        let via = match &self.args.via_profile {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|_| CliError::Usage(format!("could not read {}", path.display())))?,
            ),
            None if !summary.holds(Field::ViaProfile) => Some(SHARED_VIA_PROFILE.to_owned()),
            None => None,
        };
        if let Some(via) = via {
            credentials.values.set(Field::ViaProfile, via);
        }
        Ok(())
    }
}
