# Channel sharing for remote agents

Status: implementation in progress. This layer implements signed grants and the owner CLI.
Task admission, wakeup authorization, and harness enforcement must land before enabling sharing.

An owner can share an agent with selected channels. Channel members can mention that agent and work together in its thread.
Agents can make explicit handoffs to other shared agents. The owner's laptop does not need to remain open.

This advances the shared workspace intent in [VISION.md](../VISION.md) and the replaceable-body contract in [VISION_REMOTE_AGENTS.md](../VISION_REMOTE_AGENTS.md).
Brokkr remains the default builder and coordinator. Other specialists participate when a task needs them.

## Current behavior

The [desktop launcher](../apps/buzz-gpui/crates/app/src/agent_plugins/remote_workspace/mod.rs) accepts owner messages and verified sibling-agent mentions.
The [Kubernetes provider](../crates/buzz-backend-kubernetes/src/sandbox.rs) forces owner-only input for each thread Sandbox.
The [harness configuration](../crates/buzz-acp/src/config.rs) rejects other response policies for bound sessions.
Consequently, a sibling mention can start a Sandbox whose harness drops the triggering message.

The [manager's saved launch path](../crates/buzz-sandbox-reaper/src/mobile_launch.rs) also requires an owner's signed mention.
Opening these gates independently would leave inconsistent startup and delivery behavior.

## Behavior contract

| Area | Contract |
| --- | --- |
| Sharing | The owner enables sharing for an agent and specific channels. Existing agents remain private until enabled. |
| Invocation | Current channel members, including verified agent members, can directly mention a shared agent. |
| Access | Shared tasks use the access explicitly granted by the owner through the agent's configured tools. |
| Context | Each conversation stays bound to its community, channel, and thread. Private conversations are not loaded into shared prompts. |
| Ownership | Sharing does not transfer ownership, credentials, configuration, approval rights, or lifecycle control. |
| Availability | A valid mention can wake a sleeping agent through the manager without the owner's desktop. |
| Attribution | The original requester and any handoff agent remain visible. A teammate's request never becomes an owner's signed instruction. |
| Cost | Work consumes the agent owner's existing namespace, session quota, and Spot capacity. |
| Revocation | Removing a sharing grant or channel membership prevents new turns. Failed authorization lookup does not broaden access. |

The sharing control should state: “Allow members of these channels to run tasks with this agent's configured tool access.”
This grants task invocation, rather than creating a separate tool sandbox for each requester.
Any narrower tool policy must be enforced by the relevant tool or service.
The owner can retain private agents or use a separate agent with team credentials.

## Authorization and wakeup

Use signed Nostr events and the existing relay transport. Do not add a feature-specific HTTP endpoint.
Define new event kinds centrally before implementation, if existing kinds cannot express the sharing grant and task provenance.

The owner publishes a grant identifying the community, agent, channels, and permitted invocation behavior.
Verify ownership through the agent's cryptographic ownership attestation; a display name or claimed owner tag is insufficient.
The owner registers the agent's launch configuration with the manager before sharing becomes available.
Launch credentials stay in the manager's namespaced storage.

Before starting a Sandbox or admitting a new shared turn, verify:

1. The original event signature, explicit agent mention, channel tag, and canonical thread binding.
2. The current owner-authorized grant for that agent and channel.
3. Current membership for the requester and the addressed agent in the same community and channel.
4. Handoff provenance and remaining work budget when the requester is an agent.
5. The agent owner's configured namespace, launch registration, quota, and image policy.

Apply the same policy at startup and in the running harness.
Retain the actual requester alongside the authorizing owner and grant.
Owner-only settings, enrollment, recovery, and lifecycle commands continue to require the owner.
A shared task cannot authorize those commands by quoting an owner or using an ownership tag.

The registered manager reads mentions using the addressed agent's existing relay identity.
The agent's channel permissions constrain those reads; the manager does not join channels as a separate member.
This reuses [server-side mention startup](../docs/server-mention-launch.md) instead of adding a relay wake-notice protocol.
The manager rechecks the current grant, membership, and signed task provenance before starting shared work.
Backfill and live wake subscriptions overlap, with bounded deduplication and reconnect recovery.
Use bounded registration and subscription sets. Reuse existing session convergence and checkpoint recovery.
A duplicate mention or concurrent start must resolve to one agent-and-thread Sandbox.

## Bound agent handoffs

An agent handoff requires an explicit mention and a valid parent task in the same channel and thread.
A conversational reply alone never starts another agent.
Derive handoff depth from verified parent events, rather than trusting a claimed counter.
An agent message or delegated workflow cannot reset the budget by claiming to be a human.

Start with these limits, subject to validation:

- Four agent handoffs per human-to-agent task. A human message addressing multiple agents creates separate initial tasks.
- At most two task admissions for one agent under that request.
- One return per handoff: the target's result, addressed to the agent that handed it the work, which wakes that agent. A return cannot be returned and does not count against these limits, so returns never exceed handoffs. The ancestry walk is bounded by twice the handoff limit.
- Existing per-turn deadlines and five primary sessions per owner.
- Existing worker quotas and 100% Spot capacity.

Enforce admission budgets across restarts and replicas, with durable task provenance or an atomic shared admission record.
Return a visible refusal when a grant, budget, or capacity check prevents work.
Preserve checkpoints and report partial outcomes when cancellation interrupts an admitted task.

## Acceptance and rollout

Exercise the complete workflow with two human owners and their agents in an isolated channel.

- A teammate can mention a shared agent and receive a response while its owner is offline.
- Two owners' shared agents can perform a bounded handoff and report results in the original thread.
- Private agents, nonmembers, removed members, another community, and unshared channels cannot invoke the agent.
- Forged ownership tags, replayed grants, ambiguous thread tags, and forged handoff counters are rejected.
- Sharing does not expose stored credentials, automatically load private conversations, or transfer lifecycle rights.
- Private-channel wakeup works without adding the manager as a channel member or granting general channel reads.
- Revocation blocks new turns; authorization outages produce retryable refusals.
- Duplicate delivery, reconnect, concurrent starts, and Spot recovery preserve convergence and work budgets.
- A reply loop reaches its limit visibly and cannot reset that limit after a restart.
- Quota exhaustion preserves existing workspaces and gives an actionable response.

Keep the feature disabled until startup, delivery, provenance, revocation, and handoff budgets pass together.
Verify mixed-version behavior: older components must refuse unsupported sharing, with a visible explanation.
Require agent review, an exercised workflow, and explicit human testing before marking implementation ready.
No IAM expansion, quota increase, or move to on-demand capacity is required.

## Owner CLI

Use the human owner's account to change sharing. The relay independently verifies the agent's current ownership attestation.
The owner and agent must already belong to the selected channel. Direct messages cannot be shared.

```bash
buzz agents share <agent-pubkey> --channel <channel-uuid>
buzz agents sharing <agent-pubkey> --channel <channel-uuid>
buzz agents unshare <agent-pubkey> --channel <channel-uuid>
```

Each command uses kind `30180`, scoped by `h` and addressed by `d=<agent>:<channel>`.
An enabled head grants invocation. A disabled head revokes it. Absence means private.
These events contain no launch configuration, credentials, or private conversation history.
