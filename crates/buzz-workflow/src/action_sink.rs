//! Action sink trait — interface for workflow side-effects.
//!
//! The relay implements [`ActionSink`] to provide direct DB access to the
//! executor, replacing the HTTP loopback pattern.

use std::future::Future;
use std::pin::Pin;

use buzz_core::tenant::CommunityId;

/// Errors from action sink operations.
#[derive(Debug, thiserror::Error)]
pub enum ActionSinkError {
    /// An input parameter is malformed (e.g. invalid UUID).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The target channel does not exist.
    #[error("channel not found: {0}")]
    ChannelNotFound(String),
    /// The target channel is archived.
    #[error("channel is archived: {0}")]
    ChannelArchived(String),
    /// Nostr event construction or signing failed.
    #[error("event construction failed: {0}")]
    EventBuild(String),
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(String),
    /// Message content is empty or whitespace-only.
    #[error("empty message content")]
    EmptyContent,
}

impl From<ActionSinkError> for crate::WorkflowError {
    fn from(e: ActionSinkError) -> Self {
        crate::WorkflowError::WebhookError(e.to_string())
    }
}

/// Stored template provenance supplied by the workflow executor.
/// External callers can construct only ordinary, unproven attribution.
#[derive(Clone, Copy)]
pub struct MessageAuthorship<'a> {
    text: &'a str,
    scheduled_literal: bool,
}

impl<'a> MessageAuthorship<'a> {
    /// Attribute an ordinary workflow message without gateway prompt authority.
    pub fn unproven(text: &'a str) -> Self {
        Self {
            text,
            scheduled_literal: false,
        }
    }

    /// Bind the exact stored template to the server-captured trigger origin.
    pub(crate) fn for_step(
        definition: &crate::WorkflowDef,
        step: &str,
        text: &'a str,
        scheduled: bool,
    ) -> Self {
        let literal = definition.steps.iter().any(|stored| stored.id == step && matches!(&stored.action, crate::ActionDef::SendMessage { text: authored, .. } if authored == text && !authored.contains("{{")));
        Self {
            text,
            scheduled_literal: scheduled
                && literal
                && matches!(definition.trigger, crate::TriggerDef::Schedule { .. })
                && definition.validate().is_ok(),
        }
    }

    /// The owner's original, unrendered template.
    pub fn template(self) -> &'a str {
        self.text
    }

    /// Whether the durable definition proves a scheduled, literal prompt.
    pub fn is_scheduled_literal(self) -> bool {
        self.scheduled_literal
    }
}

/// Interface for workflow actions that produce side effects.
///
/// Implemented by the relay to provide direct DB/event access to the executor.
/// This replaces the HTTP loopback where the executor POSTed to the relay's
/// REST API (which failed with 401 auth errors).
///
/// Returns `Pin<Box<dyn Future>>` for dyn-compatibility — required because
/// `WorkflowEngine` stores `Arc<dyn ActionSink>`.
pub trait ActionSink: Send + Sync {
    /// Post a message to a channel on behalf of a workflow owner.
    ///
    /// - `community_id`: the server-resolved community that owns the workflow
    ///   run driving this side effect. The relay-signed message is published
    ///   under *this* community, never the deployment/default tenant — the run
    ///   carries its owning community so a workflow in community B posts into B
    ///   even though the side effect has no inbound connection to bind.
    /// - `channel_id`: UUID string of the target channel
    /// - `text`: rendered message body (must not be empty/whitespace-only)
    /// - `authorship`: the stored template and executor-verified schedule provenance;
    ///   consumers must use this rather than trigger-controlled rendered output
    ///   when attaching authority-bearing metadata
    /// - `author_pubkey`: hex-encoded pubkey of the workflow owner (used for
    ///   the `p` attribution tag; the relay keypair signs the event)
    /// - `reply_to`: when `Some(event_id_hex)`, the message is posted as a
    ///   threaded reply to that event (NIP-10 root/reply tags + real thread
    ///   metadata); when `None`, it is a top-level channel message.
    ///
    /// Returns the event ID hex string on success.
    fn send_message(
        &self,
        community_id: CommunityId,
        channel_id: &str,
        text: &str,
        authorship: MessageAuthorship<'_>,
        author_pubkey: &str,
        reply_to: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<String, ActionSinkError>> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_provenance_requires_actual_schedule_and_exact_stored_literal() {
        for (trigger, text, expected) in [
            (
                serde_json::json!({"on":"schedule","interval":"60s"}),
                "@worker fixed instruction",
                true,
            ),
            (
                serde_json::json!({"on":"message_posted"}),
                "@worker fixed instruction",
                false,
            ),
            (
                serde_json::json!({"on":"webhook"}),
                "@worker fixed instruction",
                false,
            ),
            (
                serde_json::json!({"on":"schedule","interval":"60s"}),
                "@worker {{trigger.text}}",
                false,
            ),
            (
                serde_json::json!({"on":"schedule","interval":"60s"}),
                "@worker {{steps.hook.output.body}}",
                false,
            ),
        ] {
            let definition: crate::WorkflowDef = serde_json::from_value(serde_json::json!({"name":"proof","trigger":trigger,"steps":[{"id":"send","action":"send_message","text":text}]})).unwrap();
            assert_eq!(
                MessageAuthorship::for_step(&definition, "send", text, true).is_scheduled_literal(),
                expected
            );
            assert!(
                !MessageAuthorship::for_step(&definition, "send", text, false)
                    .is_scheduled_literal(),
                "unproven run context cannot acquire schedule authority"
            );
            assert!(
                !MessageAuthorship::for_step(&definition, "other", text, true)
                    .is_scheduled_literal()
            );
            assert!(!MessageAuthorship::for_step(
                &definition,
                "send",
                "different stored template",
                true
            )
            .is_scheduled_literal());
        }
        let untrusted: crate::executor::TriggerContext = serde_json::from_value(serde_json::json!({"text":"","author":"","channel_id":"","timestamp":"","emoji":"","message_id":"","is_reply":false,"webhook_fields":{"scheduled":"true"}})).unwrap();
        assert!(!untrusted.scheduled);
        assert!(!MessageAuthorship::unproven("@worker fixed instruction").is_scheduled_literal());
    }
}
