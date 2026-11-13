# Shared channel agents

A human owner can let current members of one channel invoke their remote agent
and its configured tools by an explicit mention. A shared task can hand work to
another opted-in agent in the same thread. The request keeps its actual signer.
Ownership, settings, credentials, recovery, and lifecycle control remain with
the owner. DMs are not shared.

## Enable an agent

Use a CLI session authenticated as the human owner. First start the agent from
desktop once to register its launch configuration with the manager. Both the
owner and the agent must be explicit members of the channel.

```sh
buzz agents share <agent-pubkey> --channel <channel-uuid>
buzz agents sharing <agent-pubkey> --channel <channel-uuid>
```

Then another current channel member can mention the agent in a task. Sharing
allows use of the agent's configured tool access; it does not create a separate
tool sandbox for each requester. Choose the agent's credentials accordingly.

Revoke with:

```sh
buzz agents unshare <agent-pubkey> --channel <channel-uuid>
```

A verified owner can revoke after being removed from the channel or after the
channel is archived. Revocation blocks new shared turns, including queued turns
when they reach model dispatch. It does not undo work already executed.

An agent handling an admitted request can hand off specific work with its normal
agent-authenticated CLI:

```sh
buzz agents handoff <target-agent-pubkey> --from <incoming-task-event-id> --message 'Review the permission changes'
```

The target must also be opted in. Handoffs retain the original human request and
parent event. A durable manager journal permits four handoffs total and at most
two admissions per agent under one human-to-initial-agent task. A duplicate
admission consumes no additional budget. The task expires after seven days.

A managed Paseo daemon key is judged as the owner whose developer Secret holds
it, while that owner's own delegation of the key is live. Its mentions and the
handoffs in tasks it starts get exactly the owner's result, and the budget
belongs to the owner. A revoked or expired daemon key gets a permanent refusal.
Other gateway keys are judged as themselves. See
[Gateway keys](agent-gateway-key.md#shared-tasks-and-handoffs).

When the target finishes, it reports back to the agent that handed it the work:

```sh
buzz agents return --from <incoming-handoff-event-id> --message 'Merged the price feed in #123'
```

A return wakes the delegating agent with the result, so a coordinator can fan
work out and collect every result without a human. A plain @mention of an agent
does not wake it: it is refused as an agent request without task ancestry.
Each handoff returns once, and a return cannot itself be returned. Returns do not
count against the four-handoff or two-admission limits, and there can never be
more returns than handoffs. Replying to a result with new work is a handoff.

## Schedule remote work

In desktop, open **Workflows**, choose a channel, and create a workflow with a
**Schedule** trigger and a **Send message** step. Put an explicit `@AgentName`
mention and the task instructions in the saved step text. The schedule editor
supports hourly, daily, weekly, and custom cron schedules. Schedule times use UTC.

The relay runs the schedule while the owner's desktop is closed. Leave the step's
reply target unset so each fire starts a new channel thread and wakes the remote
agent through the ordinary mention listener. Register the agent from desktop once.
Keep its runtime set to Remote workspace. Both owner and agent must remain explicit
channel members. This path requires the matching manager and harness versions and the
existing `shared_tasks: true` configuration described below.

An owner's workflow can invoke their private agent without a channel sharing
grant. A teammate's workflow requires the target agent's current channel grant.
Every admission checks current ownership, channel and workspace membership, and
archive status. Existing task budgets also apply to workflow handoffs.

Only the active relay's signed workflow proof delegates the workflow owner's
authority. A mention inserted through trigger-controlled template substitutions
does not delegate it. The signed event and budget receipt keep the relay as the
actual signer; the verified workflow owner supplies the human authorization.

Disable the workflow to stop future scheduled messages. This does not cancel an
already emitted task. An unthreaded fire has a separate root and budget. Schedules
use the existing server scheduler and durable fire claims. Remote sessions use
their normal lifecycle and the configured compute and model budget.

To test after rollout, schedule a short task that replies in the channel, close
desktop, and wait for the scheduled time. Confirm a remote session starts and
replies beneath that scheduled message. Disable the workflow after the test.

## Rollout

The implementation is off by default. Before enabling it:

1. Deploy a relay that supports owner-signed kind 30180 grants.
2. Deploy the matching manager and agent images from the complete implementation
   stack. Enable ordinary mention startup with `BUZZ_SANDBOX_MENTION_LAUNCH=true`.
3. Set `shared_tasks: true` in the manager's version 1 JSON config through its
   owning infrastructure source. The manager then supplies
   `BUZZ_ACP_SHARING_MANAGER` to channel-thread launches.
4. Use a new thread or explicit recovery for a thread with older launch settings.
5. Have each human owner enable the desired channel grants, then test with a
   second human member before wider use.

Shared-capable channel sessions disable personal core memory before session
creation, including when sharing is enabled later. The manager reads using each
agent's existing identity and channel permissions. It does not join channels.
Each registered agent can subscribe to at most 64 current channels. Membership
is rediscovered on the bounded listener recycle; fresh admission also rechecks
current membership and grants. Authorization failures leave a visible refusal
or a durable retry record. A shared task never recovers an ended session; only
the owner's or a live delegated gateway's mention restores its checkpoint.

The rollout uses existing owner namespaces, quotas, and Spot capacity. Human
acceptance and staging rollout must be recorded separately from local tests.

## Local workflow evidence

Build `buzz-acp` and `buzz` from the implementation branch. Use an isolated local
relay with NIP-OA and relay membership enabled, with test owner key `33` repeated
32 times. The integration test only accepts a loopback `ws://` relay URL.

```sh
BUZZ_MENTION_TEST_RELAY=ws://127.0.0.1:13049 \
BUZZ_SHARING_TEST_ACP="$PWD/target/debug/buzz-acp" \
BUZZ_SHARING_TEST_CLI="$PWD/target/debug/buzz" \
cargo test -p buzz-sandbox-reaper shared_mentions_wake -- --ignored --nocapture
```

This runs real relay admission, wake listeners, harness processes, and CLI
commands for two owners and two agents, followed by revocation. A deterministic
ACP adapter replaces the model and a simulated Kubernetes API replaces the
cluster. It does not validate a real image pull, cloud Pod, or external model.

With the same environment variables, run
`cargo test -p buzz-sandbox-reaper scheduled_workflow_wakes -- --ignored --nocapture`
to exercise an actual relay cron fire, private-agent wake, and a reply from the
shipped harness while the owner's connection is closed. The local test uses a
one-minute cron schedule and can take approximately two minutes.
