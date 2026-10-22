//! What a failed thread-session start asks the user for, and the bounded
//! set of failures a host offers recovery for. Ports of the desktop's
//! `remoteAgentSession.ts` and `remoteSessionRecoveryPrompts.ts`.

use std::collections::VecDeque;

use crate::{SessionError, SessionScope};

/// The explicit action a thread-session error asks the user for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    /// The session ended; offer to recover it (from a checkpoint or fresh).
    Recover,
    /// A live session is in the way; offer to end it first.
    EndFirst,
    /// The session is still ending; wait for it to end.
    Ending,
}

/// The action `error` asks for, if any. The provider's refusal code decides;
/// phrase matching applies only when there is no code, for providers that
/// predate codes.
pub fn recovery_kind(error: &SessionError) -> Option<RecoveryKind> {
    if let Some(code) = &error.code {
        return match code.as_str() {
            "session_active" | "config_differs" => Some(RecoveryKind::EndFirst),
            "session_ending" => Some(RecoveryKind::Ending),
            "session_ended"
            | "pod_ended"
            | "pod_replaced"
            | "session_not_running"
            | "binding_expired"
            | "recovery_interrupted"
            | "no_checkpoint"
            | "no_ended_session" => Some(RecoveryKind::Recover),
            _ => None,
        };
    }
    let message = error.message.to_lowercase();
    if message.contains("end it first") || message.contains("end the session") {
        return Some(RecoveryKind::EndFirst);
    }
    if message.contains("session is ending") || message.contains("session is still ending") {
        return Some(RecoveryKind::Ending);
    }
    [
        "explicit recovery is required",
        "recover it explicitly once it has ended",
        "session ended",
        "start fresh instead",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
    .then_some(RecoveryKind::Recover)
}

/// Whether recovery may offer the checkpoint: not when the provider said
/// there is no verified checkpoint or no ended session to restore.
pub fn offers_checkpoint(error: &SessionError) -> bool {
    if let Some(code) = &error.code {
        return code != "no_checkpoint" && code != "no_ended_session";
    }
    let message = error.message.to_lowercase();
    !(message.contains("no verified checkpoint") || message.contains("no ended session exists"))
}

/// The most failed starts a [`RecoveryPrompts`] keeps: one per
/// (tenant, agent, thread), oldest dropped first.
pub const MAX_RECOVERY_PROMPTS: usize = 32;

/// A thread-session start that failed with a lifecycle error. The thread
/// offers the explicit recovery actions for it. Only the failure is kept:
/// the user's recovery choice is sent with one action and never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryPrompt {
    pub agent_pubkey: String,
    pub agent_name: String,
    pub session: SessionScope,
    /// Tenant scope of the failed start; the recovery action asserts it.
    pub expected_relay_url: String,
    pub expected_signer_pubkey: String,
    pub error: SessionError,
}

impl RecoveryPrompt {
    /// The identity this prompt is stored and cleared under.
    pub fn key(&self) -> RecoveryPromptKey {
        RecoveryPromptKey::new(
            &self.expected_relay_url,
            &self.expected_signer_pubkey,
            &self.agent_pubkey,
            &self.session,
        )
    }
}

/// Identity of a prompt: the tenant, the agent, and the thread. Relay and
/// pubkeys are normalized, so a later start's scope matches its failure.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RecoveryPromptKey {
    relay_url: String,
    signer_pubkey: String,
    agent_pubkey: String,
    session: SessionScope,
}

impl RecoveryPromptKey {
    /// The key for a failed start of `agent_pubkey` in `session`, under the
    /// tenant the start captured.
    pub fn new(
        expected_relay_url: &str,
        expected_signer_pubkey: &str,
        agent_pubkey: &str,
        session: &SessionScope,
    ) -> Self {
        let pubkey = |key: &str| key.trim().to_lowercase();
        Self {
            relay_url: expected_relay_url.trim().to_string(),
            signer_pubkey: pubkey(expected_signer_pubkey),
            agent_pubkey: pubkey(agent_pubkey),
            session: session.clone(),
        }
    }
}

/// Failed thread-session starts awaiting an explicit recovery choice.
/// Newest last; bounded to [`MAX_RECOVERY_PROMPTS`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryPrompts {
    prompts: VecDeque<RecoveryPrompt>,
}

impl RecoveryPrompts {
    /// Record a failed start, replacing any prompt with the same key. The
    /// oldest prompt is dropped once the bound is reached.
    pub fn record(&mut self, prompt: RecoveryPrompt) {
        let key = prompt.key();
        self.prompts.retain(|existing| existing.key() != key);
        if self.prompts.len() == MAX_RECOVERY_PROMPTS {
            self.prompts.pop_front();
        }
        self.prompts.push_back(prompt);
    }

    /// Drop the prompt for `key` after its session started, recovered, or
    /// was dismissed. Returns whether a prompt was dropped.
    pub fn clear(&mut self, key: &RecoveryPromptKey) -> bool {
        let before = self.prompts.len();
        self.prompts.retain(|existing| existing.key() != *key);
        self.prompts.len() != before
    }

    /// The prompts for one thread, oldest first.
    pub fn for_thread<'a>(
        &'a self,
        session: &'a SessionScope,
    ) -> impl Iterator<Item = &'a RecoveryPrompt> + 'a {
        self.prompts
            .iter()
            .filter(move |prompt| prompt.session == *session)
    }

    /// Every prompt, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &RecoveryPrompt> {
        self.prompts.iter()
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
