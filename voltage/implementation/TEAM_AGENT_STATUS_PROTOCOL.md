# Owner-published team status protocol

This is the first layer of [#207](https://github.com/voltagecloud/buzz/pull/207).
The owner publisher and Mission Control Team lane follow in separate dependent PRs.
This layer does not publish status automatically.

Kind `30623` is an owner-signed, channel-scoped addressable snapshot.
It does not change presence or the encrypted observer stream.

Each event has exactly six tags:

| Tag | Value |
| --- | --- |
| `d` | `team-status:COMMUNITY_UUID:AGENT_HEX:CHANNEL_UUID:ROOT_EVENT_HEX` |
| `community` | Canonical server-resolved community UUID |
| `h` | Canonical channel UUID |
| `p` | Subject agent's lowercase public key; never a mention |
| `e` | Root event ID, empty relay hint, `root` marker |
| `expiration` | Canonical decimal Unix seconds |

Content is `{"v":1,"label":"working"}` or empty content for a clear.
Labels accept `working`, `waiting`, `blocked`, or `done`, optionally followed by `: ` and an approved caption.
Content is limited to 512 bytes; labels are limited to 96 Unicode scalar values.
The parser rejects controls, line separators, active mentions, slash paths, URLs with slashes, and backtick commands.
The owner remains responsible for caption provenance and arbitrary prose that syntax cannot classify.

Publication requires the actual owner's signature and current owner and agent membership.
The root must be a live top-level conversation in the same stream or forum channel.
DMs, archived channels, mismatched communities, and missing roots are rejected.
Lifetime must be positive and at most 15 minutes; future clock skew is limited to 60 seconds.
Expired publications are rejected.

Writes serialize per owner-agent-channel across relay replicas.
The transaction applies ownership, membership, root, rate, and address checks before replacing the head.
The limits are 12 accepted writes per minute and 32 active thread addresses per owner-agent-channel.
A separate ceiling of 1,024 retained addresses per tuple bounds address growth under ordinary event retention.
Rejected writes return visible errors; publishers must retain pending clears for retry.

NIP-33 ordering selects the newest timestamp, then the lowest ID for same-second ties.
Clear and expired heads remain ordering watermarks.
Delete requests for status heads are rejected; publish empty content instead.
Expiry controls display freshness, not storage deletion.
Readers must select the winning head before hiding clear or expired content.

Status reads require an explicit kind or event ID filter and current channel access.
Snapshot queries, counts, and live delivery recheck the channel, root, owner, and subject membership.
A `p` filter selects the subject without using the mention index.
Broad message feeds, search results, mention delivery, and workflow triggers exclude status events.

## Validation

Protocol unit tests cover envelope ambiguity, community binding, content limits, clear parsing, expiry, and explicit subscriptions.
The PostgreSQL journey exercises ingestion, authorized and unauthorized reads, counts, clear/replay ordering, deletion refusal, and membership revocation.
Run the full changed-package suites and the PostgreSQL test lane before declaring this layer ready.

## Remaining layers

The owner publisher must implement explicit opt-in, preview, monotonic timestamps, durable clear retries, and approved scope retention.
The Team lane must handle expiry, deletion and membership invalidation, community switching, and overlapping snapshots/subscriptions.
Neither layer may consume observer telemetry to generate captions.
