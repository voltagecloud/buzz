//! Exact secret values the harness knows, redacted from a transcript before
//! anything is recorded.
//!
//! The agent inherits the harness environment, and its tool shell gets the
//! agent key and any forwarded credentials, so a tool title, input, or output
//! can echo them. Redaction replaces only exact values of known secret
//! variables and the agent key's two encodings. It never guesses at content.

use nostr::ToBech32;
use serde_json::Value;

/// What a redacted secret becomes.
pub(crate) const REDACTED: &str = "[redacted]";
/// Shorter values are never treated as secrets, so a variable such as
/// `X_TOKEN=1` cannot mangle every `1` in a transcript.
const MIN_SECRET_BYTES: usize = 12;
/// Name suffixes of secret variables, for example `BUZZ_PRIVATE_KEY`,
/// `ANTHROPIC_API_KEY`, `GITHUB_TOKEN`, `GH_TOKEN`, or
/// `AWS_SECRET_ACCESS_KEY`.
const SECRET_SUFFIXES: [&str; 7] = [
    "_PRIVATE_KEY",
    "_API_KEY",
    "_TOKEN",
    "_SECRET",
    "_SECRET_KEY",
    "_SECRET_ACCESS_KEY",
    "_PASSWORD",
];

/// Whether `name` is a variable whose value is a secret.
fn is_secret_name(name: &str) -> bool {
    SECRET_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The secret values one harness redacts from its transcripts.
#[derive(Debug, Default)]
pub(crate) struct Secrets {
    /// Distinct values, longest first, so a secret that contains another is
    /// replaced whole.
    values: Vec<String>,
}

impl Secrets {
    /// The agent key (hex and `nsec`) plus every value of a secret variable
    /// in `env`. Pass every environment the agent or its tools receive.
    pub(crate) fn collect(
        keys: Option<&nostr::Keys>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut values: Vec<String> = env
            .into_iter()
            .filter(|(name, _)| is_secret_name(name))
            .map(|(_, value)| value.trim().to_owned())
            .collect();
        if let Some(keys) = keys {
            values.push(keys.secret_key().to_secret_hex());
            // Bech32 encoding of a valid secret key cannot fail.
            let Ok(nsec) = keys.secret_key().to_bech32();
            values.push(nsec);
        }
        values.retain(|value| value.len() >= MIN_SECRET_BYTES);
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Self { values }
    }

    /// Longest secret, in bytes (0 when there is none).
    pub(crate) fn longest(&self) -> usize {
        self.values.first().map_or(0, String::len)
    }

    /// `text` with every secret replaced, or `None` when it holds none.
    pub(crate) fn redact_str(&self, text: &str) -> Option<String> {
        let mut found = self.values.iter().filter(|secret| text.contains(*secret));
        let first = found.next()?;
        let mut redacted = text.replace(first.as_str(), REDACTED);
        for secret in found {
            redacted = redacted.replace(secret.as_str(), REDACTED);
        }
        Some(redacted)
    }

    /// Replace every secret in every string leaf of `value`.
    pub(crate) fn redact(&self, value: &mut Value) {
        if self.values.is_empty() {
            return;
        }
        match value {
            Value::String(text) => {
                if let Some(redacted) = self.redact_str(text) {
                    *text = redacted;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact(item)),
            Value::Object(map) => map.values_mut().for_each(|item| self.redact(item)),
            _ => {}
        }
    }

    /// Redact what was appended to `buf` at byte `appended`, where
    /// `buf[..appended]` was already redacted. Only a secret that ends past
    /// `appended` can be new, so the scan starts one secret length before it,
    /// but never before `floor` (a char boundary the caller must not rewrite).
    pub(crate) fn redact_appended(&self, buf: &mut String, appended: usize, floor: usize) {
        if self.values.is_empty() {
            return;
        }
        let start = crate::floor_char_boundary(
            buf,
            appended.saturating_sub(self.longest().saturating_sub(1)),
        )
        .max(floor);
        if let Some(redacted) = self.redact_str(&buf[start..]) {
            buf.truncate(start);
            buf.push_str(&redacted);
        }
    }
}
