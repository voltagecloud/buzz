# NIP-TA: Relay terminal attachment

This protocol grants temporary shell access to one remote-agent sandbox through the existing relay.
The SDK layer defines signed grants, metadata-only audit records, encrypted frames, and bounded reliable delivery.
Runtime and CLI consumers follow in a separate stacked PR; this layer does not open terminals.

| Kind | Storage | Purpose |
| --- | --- | --- |
| `30624` | Addressable | Pending request, owner approval, acceptance, and end metadata |
| `24203` | Ephemeral, recipient gated | NIP-44 encrypted terminal frames |

All events carry `h` for the channel, `e` for the immutable thread root, and exactly one recipient `p`.
Requests and owner approvals address the sandbox agent; audit recipients identify the requester.
The `d` tag is `terminal:<session UUID>:request`, `:grant`, `:accepted`, or `:ended`.
These events describe an attachment; they are not conversation replies or thread-counter increments.

A request contains version 1, mode `terminal`, the session UUID, immutable scope, and expiration.
Its NIP-40 `expiration` tag must match the body.
The signature must be valid, creation cannot be in the future, and expiration must fall within one hour of creation.
The harness compares channel, root, and agent to its immutable binding.
It requires an owner-signed, per-session approval naming the requester and pending request ID.
The requester must be the owner or a currently delegated gateway, excluding the agent's own key.
A live global gateway delegation permits requesting approval; it never permits self-authorizing a shell.
The owner, agent, and requester must remain posting members in a current relay-signed kind 39002 roster.
The grant does not confer relay or channel membership.

Before opening a shell, consumers must strongly read the current owner approval, pending request, membership roster, and gateway delegation through the same community relay.
Missing, replaced, deleted, expired, or malformed records and read failures deny access.
Consumers repeat those checks during attachment and close when authorization ends.
The owner revokes an individual approval through NIP-09 deletion; the requester may delete its pending request.
Durable acceptance makes each approval single-use; a new PTY requires a new request and owner approval.
The acceptance and end records contain the signed metadata-only owner approval and byte counters.
No keystrokes or file bytes belong in stored events.

Frames contain the signed approval ID, sequence number, cumulative acknowledgement, and packet.
Packets are `open`, `data`, `resize`, `close`, or `ack`.
Data uses canonical base64 and decodes to at most 16 KiB.
Terminal dimensions range from 1 through 1000.
Recipients verify signature, sender, recipient, channel, thread, approval ID, and a maximum frame age of 15 seconds.

Each direction permits four unacknowledged frames and at most 64 KiB of data.
Sequence gaps are detected; duplicates are acknowledged without repeating input.
Acknowledgement-only frames use sequence zero and are never themselves acknowledged.
Retries re-encrypt after one second, producing fresh Nostr event IDs.
After five retries, the transport reports failure and consumers close the attachment.
A separate admission budget limits every direction to 64 frames per second, including acknowledgements and retries.
Consumers must apply that budget before cryptographic work.

Shell processes, idle expiry, authorization refresh, audit publication, and local TTY restoration belong to the runtime consumer.
The relay remains the only channel into the sandbox; this protocol adds no broker, exec endpoint, or tunnel.

Read-only workspace snapshots use the distinct [NIP-SA](NIP-SA.md) capability; terminal approvals never authorize snapshot export.
