//! The terminal boundary: questions, hidden secret input, and progress. The
//! wizard talks only to [`Prompter`], so tests script the answers and read
//! back everything that would have been shown.

use std::io::{BufRead, IsTerminal, Write};

use zeroize::Zeroizing;

use crate::error::CliError;

/// Asks the developer and tells them what happened.
pub trait Prompter {
    /// Whether questions can be asked. When not, every question falls back
    /// to its default or fails with its hint.
    fn interactive(&self) -> bool;

    /// Shows a line of progress.
    fn say(&mut self, text: &str);

    /// Asks a question. Without an answer it returns `default`; when not
    /// interactive it fails with `hint` unless there is a default.
    fn ask(
        &mut self,
        question: &str,
        default: Option<&str>,
        hint: &str,
    ) -> Result<String, CliError>;

    /// Asks for a secret without echoing it. When not interactive it fails
    /// with `hint`. An empty answer is returned as empty.
    fn secret(&mut self, question: &str, hint: &str) -> Result<Zeroizing<String>, CliError>;

    /// Asks a yes or no question; `default` when unanswered or not
    /// interactive.
    fn confirm(&mut self, question: &str, default: bool) -> Result<bool, CliError> {
        if !self.interactive() {
            return Ok(default);
        }
        let suffix = if default { "[Y/n]" } else { "[y/N]" };
        let answer = self.ask(&format!("{question} {suffix}"), Some(""), "")?;
        Ok(match answer.trim().to_ascii_lowercase().as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        })
    }
}

/// The real terminal: questions and progress on stderr, answers on stdin,
/// secrets through the TTY without echo.
pub struct Terminal {
    interactive: bool,
}

impl Terminal {
    /// Interactive only when stdin and stderr are terminals and `--yes` is
    /// not set.
    pub fn new(non_interactive: bool) -> Self {
        Self {
            interactive: !non_interactive
                && std::io::stdin().is_terminal()
                && std::io::stderr().is_terminal(),
        }
    }
}

impl Prompter for Terminal {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn say(&mut self, text: &str) {
        eprintln!("{text}");
    }

    fn ask(
        &mut self,
        question: &str,
        default: Option<&str>,
        hint: &str,
    ) -> Result<String, CliError> {
        if !self.interactive {
            return default
                .map(str::to_owned)
                .ok_or_else(|| CliError::Usage(format!("{}: {hint}", question.trim())));
        }
        match default.filter(|default| !default.is_empty()) {
            Some(default) => eprint!("{question} [{default}]: "),
            None => eprint!("{question}: "),
        }
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|_| CliError::Other("could not read the answer".into()))?;
        let answer = line.trim();
        Ok(if answer.is_empty() {
            default.unwrap_or_default().to_owned()
        } else {
            answer.to_owned()
        })
    }

    fn secret(&mut self, question: &str, hint: &str) -> Result<Zeroizing<String>, CliError> {
        if !self.interactive {
            return Err(CliError::Usage(format!("{}: {hint}", question.trim())));
        }
        rpassword::prompt_password(format!("{question} (hidden): "))
            .map(|text| Zeroizing::new(text.trim().to_owned()))
            .map_err(|_| CliError::Other("could not read the hidden answer".into()))
    }
}

/// A secret's shape for display: its first few characters and its length,
/// never enough to use it.
pub fn redacted(secret: &str) -> String {
    let prefix: String = secret
        .chars()
        .take_while(|c| *c != '-' && *c != '_')
        .collect();
    let shown = if prefix.len() <= 4 && prefix.len() < secret.len() {
        let end = secret
            .char_indices()
            .nth(prefix.chars().count() + 1)
            .map_or(secret.len(), |(index, _)| index);
        &secret[..end]
    } else {
        ""
    };
    format!("{shown}… ({} characters)", secret.chars().count())
}
