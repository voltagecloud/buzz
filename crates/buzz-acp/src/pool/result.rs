use super::*;

/// Return `agent` to the pool via `result_tx`, clearing any steer receiver first.
///
/// Every path that returns an `OwnedAgent` to the pool via `PromptResult` goes
/// through this function. Panic/abort paths do not — and don't need to, since a
/// panicked task's agent is never sent back via `PromptResult`.
///
/// Clearing `steer_rx` here — rather than per-arm — makes the `install_steer_rx`
/// invariant (`steer_rx.is_none()` at dispatch) structurally unviolatable: a receiver
/// installed for a turn that ends before the read loop's `take()` (e.g. session-create
/// error) is always dropped before the agent re-enters the pool, so the next dispatch
/// can never trigger the assert.
///
/// On the happy path the read loop has already called `take()`, so this is a no-op.
pub(super) fn send_prompt_result(
    result_tx: &mpsc::UnboundedSender<PromptResult>,
    turn_id: &str,
    mut agent: OwnedAgent,
    source: PromptSource,
    outcome: PromptOutcome,
    batch: Option<FlushBatch>,
) {
    agent.acp.clear_steer_rx();
    // The turn's transcript recorder stays with its guard in the finished
    // task; traffic after this point belongs to no turn.
    agent.acp.set_transcript(None);
    let _ = result_tx.send(PromptResult {
        agent,
        source,
        turn_id: turn_id.to_owned(),
        outcome,
        batch,
    });
}
