//! Bounded task admissions. Persist a successful mutation atomically before any
//! launch or turn; retries of the same (task, agent) consume no further budget.
//!
//! A return is a handoff back to the agent that sent its parent handoff: the
//! result of delegated work, waking the delegator. Each handoff allows one
//! return and a return cannot be returned, so returns never exceed handoffs;
//! they do not count against the handoff or per-agent limits.

use super::{policy::Task, task::Scope};
use nostr::{EventId, PublicKey};
use serde::{Deserialize, Serialize};

/// Maximum lifetime of one human request's handoff budget.
pub const LIFETIME_SECONDS: u64 = 7 * 24 * 60 * 60;
pub(crate) const MAX_HANDOFFS: usize = 4;
const MAX_AGENT_TASKS: usize = 2;

/// A fixed refusal, safe to report without exposing any credential or task text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A fresh human mention is required after the original request expires.
    #[error("task expired; ask a human to mention the agent again")]
    Expired,
    /// The agent-to-agent budget has been exhausted.
    #[error("four agent handoffs already admitted; ask a human before continuing")]
    Handoffs,
    /// One agent has already handled two tasks under this request.
    #[error("agent already admitted twice for this request; ask a human before continuing")]
    RepeatedAgent,
    /// A handoff must refer to an admitted incoming task for its signer.
    #[error("handoff parent was not admitted for the sending agent")]
    Parent,
    /// The delegated work already reported back once.
    #[error("this handoff was already returned; ask a human before continuing")]
    Returned,
    /// Stored provenance does not match the verified task.
    #[error("task admission journal is invalid")]
    Journal,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    event: EventId,
    agent: PublicKey,
    requester: PublicKey,
    handoff: bool,
    /// For a return, the handoff it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    returns: Option<EventId>,
}

impl Receipt {
    /// Counts against the handoff and per-agent limits.
    fn budgeted(&self) -> bool {
        self.returns.is_none()
    }
}

/// The durable admission record for one initial human-to-agent task.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    version: u32,
    scope: Scope,
    request: EventId,
    agent: PublicKey,
    human: PublicKey,
    owner: PublicKey,
    requested_at: u64,
    receipts: Vec<Receipt>,
}

impl Budget {
    /// Start from the verified human task. Seeding its initial receipt lets an
    /// owner-only agent make a handoff without having used shared admission itself.
    pub fn new(task: &Task, now: u64) -> Result<Self, Error> {
        let budget = Self {
            version: 1,
            scope: task.scope,
            request: task.origin.request,
            agent: task.origin.agent,
            human: task.human,
            owner: task.origin_owner,
            requested_at: task.requested_at,
            receipts: vec![Receipt {
                event: task.origin.request,
                agent: task.origin.agent,
                requester: task.origin_requester,
                handoff: false,
                returns: None,
            }],
        };
        budget.validate(task, now)?;
        Ok(budget)
    }

    /// The stable storage key. Each explicit initial agent has its own task.
    pub fn key(task: &Task) -> String {
        format!("{}:{}", task.origin.request, task.origin.agent)
    }

    /// Whether this record can be pruned. The original signed request remains
    /// ineligible after pruning, preventing a restart from replenishing its budget.
    pub fn expired(&self, now: u64) -> bool {
        now >= self.requested_at.saturating_add(LIFETIME_SECONDS)
    }

    fn validate(&self, task: &Task, now: u64) -> Result<(), Error> {
        if self.expired(now) || self.requested_at > now.saturating_add(60) {
            return Err(Error::Expired);
        }
        if self.version != 1
            || self.scope != task.scope
            || self.request != task.origin.request
            || self.agent != task.origin.agent
            || self.human != task.human
            || self.owner != task.origin_owner
            || self.requested_at != task.requested_at
            || self.receipts.is_empty()
            || self.receipts.iter().filter(|r| r.budgeted()).count() > MAX_HANDOFFS + 1
            || self.receipts.len() > 2 * MAX_HANDOFFS + 1
        {
            return Err(Error::Journal);
        }
        let first = &self.receipts[0];
        if first.event != self.request
            || first.agent != self.agent
            || first.requester != task.origin_requester
            || first.handoff
            || first.returns.is_some()
        {
            return Err(Error::Journal);
        }
        for (index, receipt) in self.receipts.iter().enumerate() {
            if (index > 0 && !receipt.handoff)
                || self.receipts[..index]
                    .iter()
                    .any(|r| r.event == receipt.event && r.agent == receipt.agent)
                || self
                    .receipts
                    .iter()
                    .filter(|r| r.agent == receipt.agent && r.budgeted())
                    .count()
                    > MAX_AGENT_TASKS
            {
                return Err(Error::Journal);
            }
            if let Some(answered) = receipt.returns {
                let earlier = &self.receipts[..index];
                let valid = earlier.iter().any(|r| {
                    r.event == answered
                        && r.handoff
                        && r.budgeted()
                        && r.agent == receipt.requester
                        && r.requester == receipt.agent
                });
                if !valid || earlier.iter().any(|r| r.returns == Some(answered)) {
                    return Err(Error::Journal);
                }
            }
        }
        Ok(())
    }

    /// Reserve a task in memory; caller must persist the resulting record with
    /// compare-and-swap before running it. Returns false for an identical retry.
    pub fn admit(&mut self, task: &Task, now: u64) -> Result<bool, Error> {
        self.validate(task, now)?;
        if let Some(prior) = self
            .receipts
            .iter()
            .find(|r| r.event == task.trigger && r.agent == task.agent)
        {
            return if prior.requester == task.requester && prior.handoff == task.handoff {
                Ok(false)
            } else {
                Err(Error::Journal)
            };
        }
        let Some(parent) = self
            .receipts
            .iter()
            .find(|r| r.event == task.origin.parent && r.agent == task.requester)
            .filter(|_| task.handoff)
        else {
            return Err(Error::Parent);
        };
        // Back to the agent that delegated this work: its result.
        if parent.handoff && parent.budgeted() && parent.requester == task.agent {
            let answered = parent.event;
            if self.receipts.iter().any(|r| r.returns == Some(answered)) {
                return Err(Error::Returned);
            }
            self.receipts.push(Receipt {
                event: task.trigger,
                agent: task.agent,
                requester: task.requester,
                handoff: true,
                returns: Some(answered),
            });
            return Ok(true);
        }
        if self.receipts.iter().filter(|r| r.budgeted()).count() > MAX_HANDOFFS {
            return Err(Error::Handoffs);
        }
        if self
            .receipts
            .iter()
            .filter(|r| r.agent == task.agent && r.budgeted())
            .count()
            >= MAX_AGENT_TASKS
        {
            return Err(Error::RepeatedAgent);
        }
        self.receipts.push(Receipt {
            event: task.trigger,
            agent: task.agent,
            requester: task.requester,
            handoff: true,
            returns: None,
        });
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
