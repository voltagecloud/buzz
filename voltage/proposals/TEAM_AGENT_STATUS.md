# Proposal: owner-published team agent status

Status: design only, awaiting tee8z's approval. No event kind, publisher, or Mission Control feature is implemented here.

Give teammates an opt-in view of coarse work status and its channel thread.
Keep the detailed owner activity feed private.

## Verified baseline

Inspected main commit `157150af89ea5b89353a8c1b76db99dd6f476372`.

| Existing surface | Verified behavior |
| --- | --- |
| [Kind registry](../../crates/buzz-core/src/kind.rs) | Kind 24200 is ephemeral owner-scoped encrypted observer telemetry. Kind 20003 reports an owner's agents' presence. |
| [Agent activity](../../apps/buzz-gpui/crates/app/src/agent_activity/mod.rs) | Owner-key decryption supplies a bounded live window and local archive. The archive stores encrypted envelopes. |
| [Presence handler](../../crates/buzz-relay/src/handlers/event.rs) | Kind 20003 accepts only `online`, `away`, or `offline`, with agent ownership checks. |
| [User status](../../crates/buzz-core/src/kind.rs) | Kind 30315 is globally stored, author-owned personal status, not channel-scoped team activity. |
| [Mission Control model](../../apps/buzz-gpui/crates/app/src/mission_control/model.rs) | Current labels use runner state, observed turns, tools, and transcripts. Those inputs cannot supply a teammate lane. |
| [Remote agent vision](../../VISION_REMOTE_AGENTS.md#the-only-tether) | Presence means conversational availability, not substrate health. |
| [Activity vision](../../VISION_ACTIVITY.md) | Show truthful, readable progress without inventing semantics. |
| [Noise policy](../../VISION.md#home-feed--notifications) | Zero is the default; users opt in. |

Ordinary channel messages already expose whatever progress an agent chooses to post.
The missing feature is a structured, coarse team status, not access to all work evidence.
The owner-only observer stream must remain owner-only.

## Decision

Add a new channel-scoped addressable event, provisionally named `KIND_TEAM_AGENT_STATUS`.
Reserve its numeric kind during implementation after checking the registry and protocol compatibility.
Use a 30000–39999 kind with replacement semantics; do not send schematic examples before registration.

| Option | Decision |
| --- | --- |
| Extend 20003 | Reject: presence is availability, uses ephemeral owner-agent projection, and cannot provide a reconnectable work snapshot. |
| Extend 30315 | Reject: current global personal-status storage does not enforce the required channel audience. |
| New channel-scoped kind | Recommend: explicit ownership, audience, replacement, expiry, and independent client support. |

This adds a snapshot through the existing event pipeline, not a new HTTP status service.
Do not assume a new integer automatically obtains channel authorization; implementation must wire every write, read, and subscription path.

## Minimal event contract

The following unsigned JSON is schematic. `KIND_TEAM_AGENT_STATUS` is a placeholder, not a valid deployed kind.

```json
{
  "kind": "KIND_TEAM_AGENT_STATUS",
  "pubkey": "OWNER_HEX",
  "created_at": 1794656400,
  "tags": [
    ["d", "team-status:AGENT_HEX:CHANNEL_UUID:ROOT_EVENT_HEX"],
    ["h", "CHANNEL_UUID"],
    ["p", "AGENT_HEX"],
    ["e", "ROOT_EVENT_HEX", "", "root"],
    ["expiration", "1794657300"]
  ],
  "content": "{\"v\":1,\"label\":\"working: DEV-2868 currency plumbing\"}"
}
```

The complete signed event uses normal ID and signature fields.
The owner signs it. The `p` tag identifies the subject agent; it is not a notification or execution request.
The `h` tag defines both the publication audience and the referenced thread's channel.
Cross-channel references and DM references are prohibited in version 1.
A thread root is mandatory. Work without an approved shareable thread has no team status.

The replacement address is community plus owner, kind, and `d`.
The `d` value must exactly match the agent, channel, and root tags.
This permits concurrent threads for one agent without exposing internal session IDs.
Allow one subject agent, one channel, and one root per event; reject ambiguous duplicates and extra payload fields.

`content` contains only schema version and a label.
The label begins with `working`, `waiting`, `blocked`, or `done`.
An optional suffix is an owner-approved public task caption, such as `DEV-2868 currency plumbing`.
Limit the complete label to 96 Unicode scalar values and the encoded content to 512 bytes.
Reject line breaks, control characters, and active mention syntax; render as plain text.
Labels must not contain file paths, URLs, commands, transcripts, or tool names.

These are publication rules, not a claim that arbitrary prose can be perfectly classified by the relay.
Version 1 uses an owner-written or owner-approved caption; it never summarizes telemetry to generate one.
The default caption is empty. The fixed enum alone remains useful.

## Ownership, consent, and privacy

Sharing defaults off per owner, agent, and channel.
The owner previews the exact label, thread reference, and recipient channel before first publication.
Show that all authorized channel readers, including future members while the event remains readable, can read the status.
Channel visibility rules define the audience; do not imply that an open channel is a closed team roster.
Publish into the task's own channel only. A private task must never appear in a broader engineering status stream.

For version 1, accept only the actual owner's signature.
Use the relay's existing owner-agent association as the ownership source, as presence does.
An arbitrary `p` tag, shared-agent invocation grant, or scheduled-workflow delegation cannot grant publishing authority.
The current association does not imply opt-in; the owner's explicit publishing action provides that consent.
Do not place owner keys in agent workers.

The owner client publishes manual transitions or fixed labels from an explicitly enabled task lifecycle integration.
That integration accepts only the approved coarse enum and reference tuple.
It cannot consume observer frames, tool arguments, tool output, or transcript text.
Remote owner-delegated publishing needs a separate narrow capability design; this proposal grants none.
If the owner's publisher is offline, the status expires even when the remote agent continues working.

Require current owner and agent membership in the referenced channel before accepting a publication.
Validate that the referenced event is a readable thread root in that channel and community.
Readers must retain current channel access for live delivery, historical queries, and direct-ID reads.
Apply these rules before delivering tags as well as content: thread IDs themselves can reveal relationships.

Do not add these events to global profile reads, ordinary message feeds, search snippets, notifications, or gateway wake handling.
The subject `p` tag must not make them available through mention-based bypasses.
If future search indexes them, it must enforce the same channel and community boundary.

Opt-out stops publication immediately and sends empty content for every active replacement address.
Empty content is a clear marker, not a new label. It retains the minimum routing tags and expiry.
Publish clears through a durable retry mechanism when offline, or report that clearing is pending.
Expiry bounds continued display when clearing cannot reach the relay.
Already received information cannot be recalled; opt-out does not promise deletion from screenshots or audit records.

## Freshness, ordering, and bounds

Proposed expiry is 15 minutes after `created_at`; refresh at most once every five minutes while sharing remains enabled.
Validate expiry greater than creation time and no more than 15 minutes later.
Reject creation times more than 60 seconds in the future and already expired publications.
Publish immediately for meaningful label changes, subject to rate limits.

Use the existing addressable-event ordering and deterministic equal-timestamp tie-break behavior.
Publish monotonically increasing timestamps for an address; serialize updates from the owner's active publisher.
Version 1 permits one publisher per owner. Multiple owner devices need coordination before enabling simultaneous automatic publishing.
Do not let a delayed or duplicate event restore a newer cleared status.
Readers must select the winning head first, then evaluate clear and expiry; never fall back to an older live label.

The relay retains a clear or expired winning head for ordering even when clients hide the card.
Hard-deleting heads requires a separate tombstone or retention design to prevent old-event replay.
Expiry limits display freshness, not storage retention or erasure.
Use ordinary channel event retention and audit policy; do not promise ephemeral storage.

Cap accepted publications at 12 per owner-agent-channel per minute and 32 active thread addresses per owner-agent-channel.
Reject excess writes with a visible error. Cap subscriber buffers and page snapshot queries.
Implementation must also bound stored address growth under the community's existing retention and event quotas.

If an event expires, display **No recent shared status**, not **offline**, **idle**, or **done**.
A `done` label is an owner's status claim, not evidence that tests passed or a PR merged.
After membership removal, evict affected client cards and reject further delivery.
If a thread is deleted or becomes inaccessible, hide the card and clear cached captions and references.
On community switch, discard all team status subscriptions and cached state for the previous community.

## Mission Control team lane

Add an optional **Team** lane alongside the owner's existing fleet view.
Subscribe only to this new kind in channels the viewer can access.
Fetch current heads and subscribe with an overlap, then deduplicate by event ID and replacement address.
Do not leave a gap between snapshot loading and live subscriptions.

Group rows by owner, then agent; show each shared thread once.
Each row shows the coarse label, channel/thread link, and last shared timestamp.
Provide explicit empty, expired, loading, and read-error states.
Only enable navigation when the thread remains accessible.

Reuse basic layout, profile resolution, and navigation from Mission Control.
Use a separate team status model instead of the existing transcript-derived `Live` and `OpenTurn` model.
No teammate row offers transcript expansion, raw observer data, tool details, usage, estimated effort, or runner controls.
Do not apply existing stuck-time heuristics to coarse team statuses.

The lane is read-only and does not send notifications or wake agents.
Use accessible labels and keyboard navigation; distinguish freshness with text, not color alone.
Users opt into displaying the lane independently from owners opting into publication.

## Non-goals

- Tool telemetry, file paths, transcripts, reasoning, logs, tokens, costs, or internal session identifiers.
- Decrypting, forwarding, or weakening access to kind 24200.
- Team rankings, hours worked, productivity scores, or inferred inactivity.
- Remote process health, substrate monitoring, or teammate lifecycle controls.
- A complete team activity history, cross-channel aggregation of private work, or automatic task assignment.
- Changing 20003 presence semantics or globally exposing 30315 extensions.

## Delivery plan and acceptance checks

1. Approve the contract, audience model, expiry, and owner-publisher limitation with tee8z.
2. Reserve the kind and implement validation, ownership, scoped reads, replacement, and expiry handling in a dedicated protocol change.
3. Add an owner-facing opt-in publisher with preview, clear, retry, and fixed-label lifecycle integration.
4. Add the read-only Mission Control team model and lane after the protocol is verified.

These are future implementation stages, not authorized implementation work or a requirement for stacked PRs.
The digest proposal is independent and does not depend on this event.

| Acceptance scenario | Expected result |
| --- | --- |
| Default settings | No shared status events and no unsolicited lane notifications. |
| Owner enables one agent/thread | Authorized teammate sees only the approved coarse label and reference. |
| Non-owner signs; ownership is forged in tags | Relay rejects the event. |
| Shared-agent invocation or workflow authority is reused | It grants no status-publishing authority. |
| Non-member queries by ID, `p`, `d`, or subscription | Neither content nor reference tags are disclosed. |
| Same keys and channel IDs exist in another community | No cross-community delivery or cache reuse. |
| Private task points at a broader channel | Relay rejects the reference mismatch. |
| Extra telemetry fields, excessive content, duplicate scope tags | Relay rejects the payload. |
| Clear, expiry, duplicate, and out-of-order updates | Old status never reappears as the current head. |
| Publisher offline or opt-out while offline | Pending clear is visible to owner; readers age out within the expiry window. |
| Channel removal, thread deletion, community switch | Team cards and derived references are removed. |
| Multiple threads for one agent | Separate cards; clearing one does not erase another. |
| Snapshot and live updates race | No lost transition or duplicate card. |
| Unavailable reads and expired statuses | Explicit unknown state; no inferred idle or failure. |
| Keyboard and screen reader navigation | Each row has one actionable label and an accessible thread link. |

Future protocol checks must exercise real relay write, query, and subscription paths with two owners and multiple channel audiences.
Future UI checks must exercise actual owner opt-in and teammate rendering, including revocation and expiry.

## Owner decisions and proposal validation

Recommended approval: new kind, channel-local audience, owner-only publisher, 15-minute freshness, fixed coarse labels, and optional approved caption.
The material tradeoff is that remote work loses fresh status when the owner's publisher disconnects.
A later owner-delegated publisher can address that without expanding observer access.

This document was checked against the cited source paths and vision documents.
No runtime feature, event registration, migration, or live status publication is included.
Content, links, and diff checks apply to this proposal; implementation and integration tests remain future acceptance work.
