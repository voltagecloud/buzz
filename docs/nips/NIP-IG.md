# NIP-IG: integration occurrences and delivery plans

An integration publisher mirrors external activity as signed, channel-scoped
kind 9 messages. The relay applies ordinary membership, threading, search, and
thread-counter rules. The relay contains no provider-specific webhook handlers.

| Event | Purpose |
| --- | --- |
| Kind 9 root | A stable external thread anchor with `h`, `ext-source`, and `ext-thread`. It has no `ext-action`. |
| Kind 9 reply | One occurrence, with both NIP-10 root and reply references to the anchor, plus typed `ext-*` metadata. |
| Kind 30624 | Parameterized replaceable write-ahead plan. `d` identifies the journal coordinate; `h` scopes access; `message` names the planned kind 9 event. |

Kind 30624 is a Buzz-specific addressable kind (`30000 <= n < 40000`). Before
it was assigned, the upstream nostr NIPs kind table (`nostr-protocol/nips`
`README.md` at `bec0fc89e75645e77a06d145598246efca844457`) assigned only
`30617` and `30618` in `30610`–`30629`. The nostrbook.dev registry had no
entry for `30624`. `crates/buzz-core/src/kind.rs` already used `30620`–`30622`,
and the team agent status proposal uses `30623`.

A record's content is standard-base64 JSON:

```json
{"version":1,"payload_sha256":"<canonical payload hash or null>","event":{"id":"...","pubkey":"...","created_at":0,"kind":9,"tags":[],"content":"...","sig":"..."}}
```

The decoded plan is bounded to 24 KiB. Base64 keeps duplicated message words
out of the record's lexical representation; it provides no encryption.
Normal conversation search selects kind 9. Records remain readable to channel
members and must receive the same retention controls as their messages.

Coordinates distinguish thread anchors, provider delivery IDs, payload
fingerprints, Linear issue routes, and the Linear route channel marker. Delivery and fingerprint records are
committed before the display message. The fingerprint prevents replay under
an altered delivery header, which raw-body provider signatures do not bind.
Linear transport `webhookTimestamp` is excluded from canonical occurrence
identity so a refreshed delivery timestamp does not duplicate its message.

Before replay, the publisher verifies the record and planned event signatures,
its publisher identity, channel, coordinate, payload hash, and `message` ID.
It reads records and exact message IDs with `consistency: strong`. Lookup
failure propagates; it never authorizes a new plan. An accepted event is reused.
An unaccepted plan outside the relay timestamp window receives a fresh timestamp
only after an authoritative empty ID lookup; its new plan precedes publication.
If recovery replaces a missing anchor, every unaccepted reply is rebound and
re-signed for that anchor, even when the reply's timestamp is still fresh.

Run one publisher replica with a Recreate deployment. The current implementation
serializes requests in that process. It does not coordinate competing writers
across replicas. Preserve journal records while webhook delivery can be retried.
Deleting journals or rotating the publisher key requires an operator-managed
cutover; neither operation preserves automatic deduplication. The publisher
can read a record only while it is a member of the record's `h` channel.
Removing that membership hides the record, as deleting it would. Keep the
membership while delivery can be retried, also after the channel's mapping is
removed. Linear issue routes are kept in a separate route channel, described
below.

## Linear issue routes

Linear comments usually omit the issue's team and project. A route record tells
the publisher where such a comment goes. It is a kind 30624 record at
`route:linear:<hex SHA-256 of the issue ID>`, with empty content:

| Tag | Value |
| --- | --- |
| `updated` | Signed `updatedAt` of the Issue payload it reflects, in Unix nanoseconds. |
| `route` | Channel for the issue's comments. A cleared route omits it. |
| `h` | The publisher's route channel. |

The route channel is a private channel whose only member is the publisher. It
must not be a mapped channel. A removed membership in a mapped channel
therefore hides no route, and the members of mapped channels cannot see route
metadata.

Only Issue payloads change a route. A payload that maps to a channel saves
that channel. A payload that carries a team or project but matches no mapping
clears the route, even when none was saved. A partial payload without a team,
project, or mapping changes nothing. A comment's `updatedAt` versions the
comment, not the issue, so comments never change a route.

Routes order by `updated`, then by channel: for one version, a cleared route
is lowest and the greater channel ID outranks another. A payload replaces the
saved route only when its route is greater. Every delivery order, retry, and
replay therefore converges on the newest version's decision. The publisher
saves the decision before it acknowledges the delivery. A payload that would
change a route without a valid `updatedAt` is refused. Comments without
routing metadata on a cleared issue are acknowledged and ignored.

### Relay ordering

The relay enforces the route order. A kind 30624 record is a route record
when its `d` tag is `route:<source>:<64 lowercase hex digits>`, where
`<source>` is 1 to 32 lowercase ASCII letters or digits. A route record must
carry exactly one decimal `updated` tag and at most one `route` tag with a
UUID; the relay refuses other route records. Under the coordinate's
replacement lock, the relay stores a route record only when its route is
greater than the route of the current head. It reports any other write as a
duplicate. Write times do not matter. A route write whose outcome is unknown,
also one that a stopped publisher no longer remembers, can land at any later
time and replaces only an older route. Other coordinates, including the route
plans of earlier releases below, keep ordinary addressable replacement.

When the relay reports a route write as a duplicate, the publisher reads the
saved route again. It acknowledges the delivery only when that route is at
least as great as its own.

The publisher reads an issue's route with a filter that names the route
channel in `#h`. The same filter also asks for a marker record at
`route-channel:linear` in that channel. Both records are in one channel, so
one channel access decision applies to both in a read. A read that returns
the route is used. A read that returns the marker but no route shows that the
issue has no versioned route. A read that returns neither can be hiding the
route. The publisher then writes the marker and reads again. If that read
still returns neither record, comments that need a route are refused for
retry. A publisher that the relay does not let write to the route channel
cannot save routes either, so those deliveries are also refused for retry.

Do not change the route channel while issues depend on saved routes. The
relay keys a route record by its coordinate, not its channel, so routes
cannot be copied to another channel: an equal route is refused. After a
change, the publisher does not read the routes in the earlier channel.
Comments then follow routes of earlier releases or none. A redelivered Issue
payload that is not newer than the hidden route is refused for retry. Each
issue recovers with its next newer Issue payload.

### Routes of earlier releases

Releases before version ordering saved a route as a journal plan at
`route:linear:issue:<issue ID>`. Its `h` tag names the mapped channel, and its
saved event carries `ext-route-state=unmapped` once the issue left every mapped
scope. The publisher reads such a record only when the issue has no versioned
route. The first versioned route replaces it, and a later write of the old
format cannot replace a versioned route.

Upgrade in this order and under these conditions:

1. Deploy a relay that enforces route ordering before the publisher. An
   earlier relay orders route records by write time, so a late write could
   replace a newer route.
2. Create the route channel, add the publisher as its only member, and set it
   in the publisher configuration. The publisher does not start with Linear
   mappings and no route channel.
3. Keep the publisher a member of each channel that holds an earlier route
   until the issue's next Issue payload. A hidden earlier route cannot be read,
   so comments on that issue are refused, as before the issue's first
   delivery.
4. Upgrade only when no Linear delivery is awaiting retry. The earlier records
   hold a relay write time, not the issue version they reflect, so their
   versions cannot be recovered. The first versioned route therefore replaces
   the earlier state even when its version is older. An older issue version
   that is delivered after the upgrade can set the route until the issue
   changes again.

## Source tags and workflow trust

Occurrences include `ext-source`, `ext-action`, `ext-id`, `ext-url`, `ext-actor`,
`ext-thread`, and `ext-key`. Optional fields include `ext-repo`, `ext-team`,
`ext-ref`, `ext-title`, `ext-state`, `ext-labels`, `ext-added-labels`,
`ext-conclusion`, `ext-review-state`, and `ext-channel`.

Repeated `ext-labels` / `ext-added-labels` tags describe individual labels.
Consumers fold dashes to underscores and join repeated fields with commas.
Publishers replace each comma in a label name with a space, so a joined label
field always splits back into the original labels.
These fields are publisher-controlled data, not relay attestations.

`integration_event` workflows require an explicit, nonempty `authors` allowlist
of trusted publisher keys. They match only kind 9 occurrence replies. Roots
lack `ext-action`, and journal records never trigger the same workflow again.

```yaml
name: linear-agent-work
trigger:
  on: integration_event
  source: linear
  authors:
    - "<64-character integration publisher public key>"
  filter: 'str_contains("," + trigger_added_labels + ",", ",agent,")'
steps:
  - id: delegate
    action: send_message
    reply_in_thread: true
    text: "nostr:<agent-npub> Review {{trigger.url}} and propose a plan."
```

Use the repository workflow schema for action fields and mention targeting.
Manage these workflows through `buzz workflows` from Paseo or a Buzz client.
