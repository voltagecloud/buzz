# Single-use sandbox attachment approvals

Terminal and snapshot approvals require a durable first acceptance before work starts.
The relay serializes acceptance at the existing addressable-event coordinate lock.
The acceptance coordinate is `terminal:<session UUID>:accepted` or `snapshot:<session UUID>:accepted`.
The UUID must use its canonical lowercase representation.

The agent signs the acceptance and embeds the owner's signed, scoped approval.
The relay verifies the typed approval, immutable scope, agent ownership, and unique claim nonce.
The nonce is a canonical UUID in `buzz:attach-claim`.
Each attempt creates one event and retains that exact event for retries.
Acceptance byte counters start at zero, and acceptance events cannot expire.

| Relay receipt | Consumer action |
| --- | --- |
| Accepted, matching event ID, message `sandbox-attach: first-claim-v1` | This attempt may start its approved operation. |
| Duplicate, ordinary success, rejection, timeout, or mismatched event ID | Refuse to start; request a new owner approval. |

Only the transaction that inserts the first acceptance receives the first-claim message.
Exact event retries never receive that message.
An older relay's generic success therefore cannot authorize a new consumer.
Capability discovery alone cannot establish that a particular write was atomic.
After an ambiguous receipt, consumers must fail closed even when a later query finds their event.

The existing `parameterized_event_watermarks` table retains the consumed coordinate in the same transaction.
Deleting an audit body does not reopen its coordinate.
NIP-09 event or address deletion and NIP-29 message deletion preserve acceptance records.
Pending requests and owner approvals retain ordinary deletion and replacement behavior.
Consumers must still check their current approval, pending request, membership, and delegation before and during work.

Acceptance and end events contain signed approval metadata and byte counters.
They must never contain keystrokes, terminal output, or snapshot file bytes.
Operators may remove event bodies through trusted retention procedures; the consumed-coordinate watermark must remain.
Community deletion purges the existing tenant-scoped watermark table through the normal deletion path.
Trusted operator removal of that watermark resets this replay boundary and requires retiring the affected approval.
This protocol does not prevent an operator with direct database access from resetting authority state.

Run the production-ingest regression against a disposable PostgreSQL database:

```sh
DATABASE_URL=postgres://buzz:buzz_dev@127.0.0.1:55432/buzz \
  cargo test --locked -p buzz-relay --lib \
  handlers::attach_claim::postgres_tests -- --ignored
```

Both tests must pass.
They exercise concurrent claims, duplicate receipts, deletion, metadata retention, malformed envelopes, and unowned signers for both modes.
Runtime terminal and snapshot consumers are separate layers; this relay layer does not start either operation.

See [NIP-TA](nips/NIP-TA.md) and [NIP-SA](nips/NIP-SA.md) for their distinct approvals.
