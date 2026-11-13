NIP-AT
======

Agent Session Transcripts
-------------------------

`draft` `optional` `relay`

This NIP defines `kind:44201`, a durable and encrypted record of one agent
turn. At every turn end, the agent harness publishes the turn's ACP
`session/update` items, its usage, and the git checkouts it worked in. Each
copy is NIP-44 encrypted to one recipient: the agent's owner, or a gateway
the owner currently delegates to. A client can then rebuild the conversation,
with its tool calls, diffs, and plans, after the agent's sandbox is gone.

## Motivation

A remote agent's sandbox is disposable. The relay keeps the agent's chat
messages, but the detail of its work (tool calls, file locations, diffs,
plans, and token usage) streams only as [NIP-AO](NIP-AO.md) telemetry, which
relays never store. When the sandbox scales down or a client restarts, that
detail is lost.

Kind 44201 stores that detail on the relay, encrypted to the people who could
already watch it live. [NIP-AM](NIP-AM.md) and NIP-AO kept transcripts out of
durable storage. This NIP adds them on purpose, with the same encryption,
recipients, and read gating as live telemetry, and with a fixed size bound per
turn.

## Definitions

- **Agent**: an AI process with its own Nostr key, run by a harness for its
  owner.
- **Owner**: the pubkey the agent is registered under.
- **Live gateway**: a key named in the owner's current, enabled kind 30181
  delegation, as the harness resolves it
  ([agent gateway keys](../agent-gateway-key.md)).
- **Turn**: one task run by the harness, from dispatch to its end. A turn can
  send more than one ACP `session/prompt` (for example a setup prompt and the
  user's prompt).
- **Part**: one kind 44201 event. A turn has 1 to 8 parts for each recipient.

## Event

```json
{
  "kind": 44201,
  "pubkey": "<agent pubkey>",
  "created_at": <unix seconds>,
  "content": "<NIP-44 v2 ciphertext>",
  "tags": [
    ["p", "<recipient pubkey>"],
    ["e", "<thread root event id>", "", "root"],
    ["h", "<channel uuid>"],
    ["turn", "<turn id>", "<part index>", "<part count>"]
  ],
  "sig": "..."
}
```

Kind 44201 is a regular stored event. Relays never replace it.

The event MUST have exactly these four tags and no other tag:

| Tag | Value |
| --- | --- |
| `p` | The recipient: 64 lowercase hex characters. |
| `e` | The thread root: 64 lowercase hex characters, an empty relay hint, and the marker `root`. |
| `h` | The channel: a lowercase hyphenated UUID. |
| `turn` | The turn id (1 to 128 bytes of `A-Z a-z 0-9 . _ : -`), the zero-based part index, and the part count (1 to 8). Both numbers are canonical decimals, and the index is less than the count. |

The `content` MUST be NIP-44 v2 ciphertext: base64 whose first decoded byte is
`0x02`, with a length from 132 to 87,472 characters.

The `h` tag places the event in its channel, so the relay applies channel
membership to writers and readers. The event is not a thread reply: the relay
does not count it in the thread root's reply counters.

## Recipients and encryption

The harness publishes one copy of the turn for each recipient:

1. the owner;
2. each live gateway at publish time (at most two, as the gateway rules
   define).

If the gateway lookup fails, the harness publishes to the owner only. Each
part's `content` is NIP-44 v2 encryption of its payload with the key pair
(agent private key, recipient public key). There is no shared copy.

## Payload (version 1)

The content decrypts to UTF-8 JSON. A part's plaintext MUST NOT exceed
61,440 bytes (60 KiB).

```jsonc
{
  "v": 1,
  "turn_id": "c2b1…",                  // equals the turn tag's id
  "turn_seq": 7,                       // increases within one harness process
  "thread_root": "<64 hex>",           // equals the e tag
  "started_at": "2026-11-13T10:00:00.000Z",  // RFC 3339
  "ended_at": "2026-11-13T10:03:12.512Z",    // RFC 3339
  "stop_reason": "end_turn",
  "usage": {                           // omitted when unknown
    "inputTokens": 1200, "outputTokens": 340, "totalTokens": 1540,
    "cachedReadTokens": 900, "cachedWriteTokens": 0
  },
  "context": {"size": 200000, "used": 51234},  // omitted when unknown
  "workspace": [
    {"path": "/home/agent/backend", "repo": "voltagecloud/backend",
     "branch": "feat/x", "head": "<40 or 64 hex>", "pushed": true}
  ],
  "omitted": 0,                        // omitted when zero
  "updates": [ /* ACP session/update `update` objects, in order */ ]
}
```

Every part of a turn carries the same fields. Only `updates` differs: it holds
that part's slice. A reader joins the slices in part order.

| Field | Rule |
| --- | --- |
| `v` | `1`. Readers reject other versions. |
| `turn_seq` | Increases within one harness process. Order turns by `started_at`, then `turn_seq`. |
| `stop_reason` | The ACP stop reason of the turn's last prompt (`end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled`). It is `error` when that prompt got an error or no answer, and `unknown` for a malformed value. 1 to 32 bytes of `a-z` and `_`. |
| `usage` | Numeric token counts from the last prompt's response. Each field is a non-negative integer, and is omitted when the agent did not report it. Other fields, such as cost, are dropped. |
| `context` | `size` and `used` from the turn's latest ACP `usage_update`. |
| `workspace` | At most 16 entries. See [Workspace](#workspace). |
| `omitted` | The number of updates dropped to fit the size limits. See [Size limits](#size-limits). |
| `updates` | Items whose `sessionUpdate` is `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, or `plan`. Tool items have a non-empty string `toolCallId`. |

Readers MUST ignore unknown fields.

### Updates

The harness records updates from the raw ACP JSON-RPC messages it reads, the
same messages its observer telemetry carries:

- It keeps only the updates of the session that the turn's newest
  `session/prompt` names. It drops updates of other sessions, and updates that
  arrive before the first prompt (for example a `session/load` history
  replay).
- It keeps only the five kinds above. It drops `user_message_chunk`,
  `usage_update` (whose values go to `context`), and all other kinds.
- It merges consecutive text chunks of the same kind and the same
  `messageId` into one item.
- It replaces each exact secret value it knows with `[redacted]`, in every
  string of every update, before anything else. See
  [Redaction](#redaction).
- It elides every string over 8 KiB to its first and last 3,000 bytes, with
  the observer-frame marker `…[elided N bytes]…` between them, where `N` is
  the number of bytes removed.

An elided or redacted string is no longer the original text. In particular, a
`diff` content item whose `oldText` or `newText` carries the elision marker
(or `[redacted]`) is a summary for display. A reader MUST NOT apply it as a
patch.

### Redaction

The agent inherits the harness environment, and its tools receive the agent
key and any forwarded credentials. A tool title, input, or output can echo
them. Before it records an update, the harness replaces every occurrence of
these values with `[redacted]`:

- the agent's private key, as hex and as `nsec`;
- the value of each variable whose name ends in `_PRIVATE_KEY`, `_API_KEY`,
  `_TOKEN`, `_SECRET`, `_SECRET_KEY`, `_SECRET_ACCESS_KEY`, or `_PASSWORD`,
  in the harness environment, the persona environment, or the tool
  environment (for example `BUZZ_PRIVATE_KEY`, `ANTHROPIC_API_KEY`, and
  `GITHUB_TOKEN`).

Values shorter than 12 bytes are not redacted, so that a short value cannot
replace unrelated text. Redaction matches exact values only. It does not
detect secrets by their shape, and it does not find a secret in another
encoding (for example base64). A secret that the agent reads from a file is
not redacted.

### Workspace

At turn end the harness reads the git checkouts at its working directory and
directly under it, at most 16, using local refs only. It does not fetch.

- `repo` is the `owner/repo` of the `origin` remote. The harness leaves out
  checkouts without a GitHub `origin`. Credentials in the remote URL are never
  copied.
- `branch` is the current branch. It is omitted for a detached `HEAD`.
- `head` is the `HEAD` commit.
- `pushed` is true when the local `origin/<branch>` ref contains `head`. It is
  false for a detached `HEAD`.

The scan has a 2-second limit. A checkout that cannot be read is left out.

## Size limits

Each part's plaintext is at most 60 KiB, and a turn has at most 8 parts for
each recipient.

The **skeleton** of a tool item is its `sessionUpdate`, `toolCallId`,
`title`, `kind`, `status`, and `locations`, with `locations` cut to its first
32 entries and at most 8 KiB. The skeleton of a chunk is its `sessionUpdate`,
`messageId`, and `content` text. A replaced field holds an elision marker. The
harness replaces a field only when the marker is smaller than the field.

First, the harness shrinks each item that is too large for a part on its own,
so that one item never removes the others:

- a tool item loses `content`, `rawOutput`, and `_meta`, then its other
  non-skeleton fields (`rawInput` first), one field at a time, until it fits.
  If it still does not fit, it becomes its skeleton.
- a chunk becomes its skeleton.
- an item that still does not fit (for example a plan with thousands of
  entries) is dropped and counted in `omitted`.

Then, when the updates still do not fit, the harness changes them in this
order and stops as soon as they fit:

1. It replaces `content`, `rawOutput`, and `_meta` of tool items, oldest
   first.
2. It replaces every other non-skeleton field of tool items (`rawInput` and
   any other field), oldest first.
3. It drops chunk and plan items, oldest first.
4. It drops tool items, oldest first.

Steps 1 and 2 never change the skeleton fields. Steps 3 and 4 add each dropped
item to `omitted`, so a reader always knows that items are missing.

The harness also limits memory while it records: at most 2,048 items and
2 MiB. Each item is shrunk to 60 KiB as above before the harness keeps it, so
one item cannot use the budget of the others. Past 1 MiB, new tool items keep
only their skeleton. Past the hard limits, new items are dropped and counted in
`omitted`.

## Publisher behavior

- Publish once at every turn end that sent a prompt to the agent, including
  cancelled, failed, and timed-out turns. A turn refused or requeued before
  any prompt has no transcript.
- Publish only for turns in a channel thread, and only when the agent has a
  configured owner. Heartbeats and other turns without a thread root have no
  transcript.
- Publish to all recipients at the same time, so that a slow submission for
  one recipient does not delay the others. Publish each recipient's parts in
  part order.
- Submit each part up to 4 times. Each attempt has a 5-second limit, and the
  delay between attempts starts at 250 ms and doubles. A resubmission sends
  the same signed event, so the relay stores it once. The whole turn,
  including retries, has a 30-second limit.
- Publishing does not fail the turn. A part that is not published after its
  last attempt is logged and the turn stays incomplete for that recipient.
- On shutdown, the harness waits for in-flight publishes in the same grace
  period that it gives in-flight turns.
- `created_at` is the publish time.

## Relay behavior

On receiving a kind 44201 event, a relay MUST:

1. Validate the signature.
2. Reject the event unless `content` is NIP-44 v2 ciphertext (base64, version
   byte `0x02`, length from 132 to 87,472).
3. Reject the event unless its tags are exactly the four tags above.
4. Apply channel membership to the author, as for other channel events.
5. Accept the event only when the `p` recipient is the author's registered
   owner, or a gateway that the owner currently delegates to. Lookup failures
   reject the event.
6. Store the event in its channel. It is not a thread reply.

Reads are gated on every path: only an authenticated reader whose pubkey is
the `p` value receives the event. This holds for filters with `ids` too.
Knowing an event id is not authorization.

### Search

A stored kind with `p`-gated reads normally needs a NULL `search_tsv`, so
NIP-50 search cannot match it. Kind 44201 is exempt from that rule: relays
store only ciphertext for it (step 2), and indexing base64 ciphertext reveals
nothing about the plaintext.

The ciphertext rule checks shape, not secrecy: an author can submit
base64 that starts with `0x02` but is not real encryption, and that text can
then match a search. This is acceptable for two reasons. The only accepted
author is the agent, which controls its own content and can already publish
plaintext in its channel messages. And a match never reveals the event:
reads are gated on `p` on every path, search included, so only the recipient
receives it.

This exemption avoids a change to the generated
`search_tsv` column, which would rewrite the whole events table. The exempt
kinds are listed in `CIPHERTEXT_ONLY_P_GATED_KINDS` in `buzz-core`, and ingest
applies the ciphertext rule to every kind in that list.

## Reader behavior

A reader queries its own parts from the agent:

```json
{"kinds": [44201], "authors": ["<agent pubkey>"], "#p": ["<own pubkey>"], "#e": ["<thread root>"]}
```

A shared thread can have more than one agent writing transcripts. Use one
`authors` entry for each agent the reader expects.

For each part, the reader MUST:

1. verify the signature;
2. check that the author is the agent it expects;
3. check the envelope and that `p` is its own pubkey;
4. decrypt the content and reject plaintext over 60 KiB;
5. check the payload rules and that `turn_id` and `thread_root` match the
   tags.

The reader groups parts by (author, `turn_id`) and joins them in part order.
Turn ids are unique only for one author. A turn with missing parts is
incomplete. The reader can show the parts it has, or fall
back to the thread's chat messages. `buzz-sdk` implements steps 1 to 5 as
`transcript::open_agent_session_transcript`.

## Privacy

The cleartext tags show the recipient, the thread root, the channel, the turn
id, and the number of parts, and `created_at` shows when the turn ended. This
is the same class of metadata as NIP-AM: a relay operator learns that an agent
finished a turn in a thread, which its chat messages already show. Everything
else is encrypted.

The payload holds conversation content, tool inputs and outputs, and file
paths. Only the owner and the gateways the owner delegated to at publish time
can decrypt it. A gateway revoked later keeps the parts it already received.
NIP-44 gives no forward secrecy: anyone with the agent's private key can
decrypt every part. The harness redacts the secrets it knows before it records
a turn (see [Redaction](#redaction)), but other secrets that a tool prints
stay in the payload.

## Retention

Retention is an open question. This version does not define an expiration,
and the relay keeps parts until the agent deletes them (NIP-09). A later
version can add a retention period or allow an `expiration` tag.

## Relationship to other NIPs

- [NIP-AO](NIP-AO.md): the same recipients and encryption, but ephemeral and
  per frame. NIP-AT is the durable, bounded record of one turn.
- [NIP-AM](NIP-AM.md): owner-only, channel-free usage metrics. NIP-AT also
  carries the turn's usage, for each recipient.
- [NIP-09](09.md): the agent can request deletion of its parts. The relay
  applies standard deletion.
- [NIP-40](40.md): not used. An `expiration` tag is not allowed.
