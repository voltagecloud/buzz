# NIP-IG: integration occurrences and delivery plans

An integration publisher mirrors external activity as signed, channel-scoped
kind 9 messages. The relay applies ordinary membership, threading, search, and
thread-counter rules. The relay contains no provider-specific webhook handlers.

| Event | Purpose |
| --- | --- |
| Kind 9 root | A stable external thread anchor with `h`, `ext-source`, and `ext-thread`. It has no `ext-action`. |
| Kind 9 reply | One occurrence, with NIP-10 root reference and typed `ext-*` metadata. |
| Kind 30623 | Parameterized replaceable write-ahead plan. `d` identifies the journal coordinate; `h` scopes access; `message` names the planned kind 9 event. |

A record's content is standard-base64 JSON:

```json
{"version":1,"payload_sha256":"<canonical payload hash or null>","event":{"id":"...","pubkey":"...","created_at":0,"kind":9,"tags":[],"content":"...","sig":"..."}}
```

The decoded plan is bounded to 24 KiB. Base64 keeps duplicated message words
out of the record's lexical representation; it provides no encryption.
Normal conversation search selects kind 9. Records remain readable to channel
members and must receive the same retention controls as their messages.

Coordinates distinguish thread anchors, provider delivery IDs, payload
fingerprints, and Linear issue routing. Delivery and fingerprint records are
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

Run one publisher replica with a Recreate deployment. The current implementation
serializes requests in that process. It does not coordinate competing writers
across replicas. Preserve journal records while webhook delivery can be retried.
Deleting journals or rotating the publisher key requires an operator-managed
cutover; neither operation preserves automatic deduplication.

## Source tags and workflow trust

Occurrences include `ext-source`, `ext-action`, `ext-id`, `ext-url`, `ext-actor`,
`ext-thread`, and `ext-key`. Optional fields include `ext-repo`, `ext-team`,
`ext-ref`, `ext-title`, `ext-state`, `ext-labels`, `ext-added-labels`,
`ext-conclusion`, `ext-review-state`, and `ext-channel`.

Repeated `ext-labels` / `ext-added-labels` tags describe individual labels.
Consumers fold dashes to underscores and join repeated fields with commas.
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
