# Proposal: weekday engineering digest

Status: awaiting tee8z's design and creation approval. Nothing in this proposal creates an agent or workflow.

The relay schedules a literal prompt. A dedicated agent reads approved conversations and writes one digest in engineering.

## Approval package

- [Workflow definition](workflow.yaml): weekdays at 09:00 UTC, disabled for review.
- [Agent prompt](AGENT_PROMPT.md): proposed display name **Engineering Digest**.
- Destination: engineering, `b25e2e33-fcbe-4db7-a726-9a6f7eb4df67`.
- Proposed owner: tee8z. The workflow must use that owner's authority, not an unrelated agent's identity.
- Initial source allowlist: engineering only. Additional channels require explicit approval for republication into engineering.
- Initial developer consent roster: empty. Absence from the roster means excluded, not implicit consent.

Approve the time, destination, source allowlist, developer roster, and agent identity before activation.
Record consent and agent-to-developer attribution by pubkey in the owner's approved agent configuration.
Do not infer consent from membership, messages, or activity.
The final configuration must name the selected agent's actual pubkey and exact display name.

The YAML is a review artifact, not an installed workflow.
After the agent exists, resolve its mention uniquely against engineering membership.
Keep the exact display-name mention: the current workflow sink resolves `@name`, not `nostr:npub...` URIs.
Verify that the resolved pubkey matches the approved agent before activation.
A rename or ambiguous name requires owner-approved configuration repair and another wake check.
Keep the resulting text literal; never interpolate a date, source message, or search result into it.

## Verified behavior

Inspected main commit `157150af89ea5b89353a8c1b76db99dd6f476372`.
Paths below refer to that revision.

| Constraint | Source and implication |
| --- | --- |
| Gateway wake authority requires a scheduled literal | [SDK verifier](../../../crates/buzz-sdk/src/workflow.rs), [action provenance](../../../crates/buzz-workflow/src/action_sink.rs). Stored text cannot contain template interpolation. |
| Mentions must exist in stored text | [Workflow sink](../../../crates/buzz-relay/src/workflow_sink.rs). Rendered routing tags alone cannot confer authority. |
| Schedule requires a channel | [Cron loop](../../../crates/buzz-workflow/src/lib.rs). Creation must bind engineering, in addition to the action's destination. |
| Cron is UTC | [Schema](../../../crates/buzz-workflow/src/schema.rs). Five-field cron is supported; `0 9 * * 1-5` matches an existing schema fixture. |
| Feed queries cap rows at 100 | [Feed store](../../../crates/buzz-db/src/store/feed.rs). A feed response cannot prove complete coverage. |
| Listed tools are presentation metadata | [Tool catalog](../../../desktop/src/features/agents/ui/agentSessionToolCatalog.ts). The catalog names `get_feed`, `get_channel_history`, and `search`; it does not register executable tools. |
| CLI feed differs | [CLI feed](../../../crates/buzz-cli/src/commands/feed.rs) caps at 50 and filters mentions. It is not a complete channel activity feed. |
| Digests require opt-in | [Vision](../../../VISION.md#home-feed--notifications): zero noise is the default. |
| Creation, update, and manual trigger need approval | [Remote agent policy](../../remote-agents.md#schedule-work). Merging this proposal grants none of these actions. |

No digest-specific checkpoint was found in `crates/buzz-workflow` or `crates/buzz-db/src/store/feed.rs`.
The existing scheduled-run claim prevents duplicate schedule claims; it is not a digest publication watermark.

## Window and completeness

Use fixed half-open windows `[previous weekday 09:00 UTC, scheduled weekday 09:00 UTC)`.
Monday includes Friday morning through Monday morning; other weekdays cover 24 hours.
Anchor the window to the scheduled prompt's weekday boundary, not the completion time.
If the prompt timestamp cannot identify that boundary unambiguously, request owner direction before publishing.

This needs no durable watermark and survives agent replacement.
It does not automatically recover a missed weekday run or late-arriving events with old timestamps.
List missed windows when observed; let tee8z authorize a catch-up request.
A future watermark must advance only after confirmed publication and store the resulting digest event ID.

Use the feed for discovery, then read every approved source channel even when the feed has fewer than 100 results.
Page history through the full window and fetch containing threads for context.
Deduplicate by event ID and group by canonical thread root, including older roots with new replies.
Search supplements history; relevance-ranked search is not proof of completeness.

Inspect actual tool schemas before use.
If MCP reads are unavailable, use installed `buzz messages get`, `messages thread`, and `messages search` commands.
The CLI history command supports `--since`, `--before`, and `--limit`.
Retain overlapping timestamp boundaries and deduplicate IDs when paging.
If a full page repeats at one timestamp, report incomplete coverage rather than skipping the tied events.
Cap each run at 20 source channels, 50 history pages per channel, and 10,000 unique events overall.
Any cap, permission error, missing thread, or unpageable result must appear in the coverage footer.

## Consent and attribution

Source access does not itself authorize broader disclosure.
Initially summarize engineering only, which keeps source and destination audiences aligned.
Additional channels require owner approval and source participants' consent to the stated destination audience.
Exclude DMs and private channels in this version, even when the digest agent can read them.

Include only developers who explicitly opt in, and only their approved agent identities.
Ignore opted-out participants' contributions when summarizing mixed threads.
If a useful summary would reveal those contributions, omit the thread and note limited coverage without naming the participant.
Assign a thread to its explicit consenting task owner.
If ownership is ambiguous, place it once under **Shared / owner unclear**; never guess from the last reply.
Do not report inactivity, rankings, time spent, or productivity estimates.

A dedicated agent has only the approved source memberships and destination posting access where configuration permits.
The prompt constrains behavior; it is not a replacement for relay authorization or a technical data-loss prevention boundary.
Owners can revoke consent prospectively. Previously published summaries may already have been read or copied.

## Implementation and acceptance plan

1. tee8z approves this package and the concrete roster. Create the agent through the owner-reviewed agent flow.
2. Verify available read tools, destination membership, owner delegation, and the literal mention's exact identity.
3. Obtain explicit approval to create the final channel-bound workflow and to enable its schedule.
4. After separate test approval, exercise one scheduled run in staging. A manual trigger does not prove scheduled gateway authority.
5. Check the posted digest with tee8z, then retain or disable the approved schedule.

Acceptance checks for the future activation:

| Case | Expected result |
| --- | --- |
| Weekday schedule | One literal prompt wakes the intended agent under the approved owner. |
| Interpolated or non-schedule prompt | No scheduled-literal gateway authority. |
| Monday boundary | Weekend activity is included; the upper boundary is excluded. |
| More than 100 events across channels | Per-channel history finds threads missing from the feed. |
| Pagination ties or read failures | Footer identifies incomplete coverage without claiming inactivity. |
| Multiple replies and nested threads | One line per canonical active thread. |
| Missing consent or DM/private source | No contribution is summarized or exposed. |
| Duplicate wake for the same window | Existing digest is detected; no second post when detection succeeds. |
| Ambiguous send result | Check for the digest before retrying; do not promise exactly-once delivery. |
| Revocation or lost membership | Future collection/publication stops for that scope. |

Before posting, search engineering for the exact window key from this agent.
This is best-effort deduplication, not an atomic publish lock; concurrent turns can still race.
Do not enable overlapping digest publishers. Exactly-once publication requires a separate implementation proposal.

## Validation of this proposal

Review the YAML against `WorkflowDef` and its existing cron fixture.
Check relative links and the diff. No application behavior changes or live workflow tests are part of this PR.
Human confirmation remains required before marking the proposal ready under repository policy.
