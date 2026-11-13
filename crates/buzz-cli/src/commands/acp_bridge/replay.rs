//! Merge validated transcript turns with durable conversation messages.
use super::*;
use crate::commands::acp_bridge::{views::BranchViews, workspace_paths};

/// Preserve durable posts unless the transcript contains that exact answer.
/// A tool may publish a message that differs from its ACP assistant text.
pub(super) fn updates(
    scope: &Scope,
    mut events: Vec<Event>,
    labels: HashMap<PublicKey, String>,
    turns: Vec<transcripts::ReplayTurn>,
    views: &Arc<BranchViews>,
) -> Vec<Value> {
    events.sort_by_key(|event| (event.created_at, event.id));
    events.dedup_by_key(|event| event.id);
    let answers: Vec<String> = turns
        .iter()
        .map(|turn| {
            turn.payload
                .updates
                .iter()
                .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
                .filter_map(|update| update["content"]["text"].as_str())
                .collect()
        })
        .collect();
    let mut pending = turns.iter().peekable();
    let mut output = Vec::new();
    let mut agent_side = false;
    for event in events {
        while pending.peek().is_some_and(|turn| {
            turn.started < event.created_at.as_secs()
                || (turn.started == event.created_at.as_secs() && event.pubkey == scope.agent)
        }) {
            if let Some(turn) = pending.next() {
                append_turn(turn, scope, views, &mut output, &mut agent_side);
            }
        }
        if event.pubkey == scope.agent
            && turns.iter().zip(&answers).any(|(turn, answer)| {
                turn.covers(event.created_at.as_secs())
                    && !answer.is_empty()
                    && answer == &event.content
            })
        {
            continue;
        }
        let own = event.pubkey == scope.owner || event.pubkey == scope.signer.public_key();
        let update = if own {
            let mut update = wire::agent_text_chunk(&event.content);
            update["sessionUpdate"] = json!("user_message_chunk");
            update
        } else if event.pubkey != scope.agent {
            wire::agent_text_chunk(&format!(
                "\n\n[{}] {}\n\n",
                labels
                    .get(&event.pubkey)
                    .cloned()
                    .unwrap_or_else(|| names::fallback(event.pubkey)),
                event.content
            ))
        } else if agent_side {
            wire::agent_text_chunk(&format!("\n\n{}", event.content))
        } else {
            wire::agent_text_chunk(&event.content)
        };
        agent_side = !own;
        output.push(update);
    }
    for turn in pending {
        append_turn(turn, scope, views, &mut output, &mut agent_side);
    }
    output
}

fn append_turn(
    turn: &transcripts::ReplayTurn,
    scope: &Scope,
    views: &Arc<BranchViews>,
    output: &mut Vec<Value>,
    agent_side: &mut bool,
) {
    for mut update in turn.payload.updates.clone() {
        if update["sessionUpdate"] == "agent_message_chunk" && *agent_side {
            output.push(wire::agent_text_chunk("\n\n"));
            *agent_side = false;
        }
        for view in
            workspace_paths::prepare_update(&mut update, &turn.payload.workspace, &scope.home)
        {
            views.request(view);
        }
        output.push(update);
    }
    if turn.payload.omitted > 0 {
        output.push(wire::agent_text_chunk(&format!(
            "\n[Transcript omitted {} updates to fit its storage limit.]\n",
            turn.payload.omitted
        )));
    }
    if let Some(usage) = turn.usage_update() {
        output.push(usage);
    }
    *agent_side = true;
}
