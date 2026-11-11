# NIP-SA: Owner-approved workspace snapshots

This protocol defines snapshot approvals, encrypted result receipts, upload recovery metadata, and bounded Git-bundle preflight.
The [agent runtime](../sandbox-snapshot-runtime.md) creates and uploads scanned snapshots.
The [CLI workflow](../sandbox-snapshot-cli.md) requests approval, restores a new read-only directory, and returns patches.

Snapshots use kind 30624 with `h`, `e`, and one `p`, as NIP-TA does.
Their `d` coordinates are `snapshot:<session UUID>:request`, `:grant`, `:accepted`, `:ready`, and `:ended`.
The body mode is `snapshot`; terminal parsers refuse it.
Separate SDK types prevent passing a snapshot approval into a terminal transport API.

The requester publishes a signed pending request naming one channel, immutable thread root, agent, and expiration.
The owner must sign a separate per-session approval naming that requester and pending request ID.
A live gateway delegation permits requesting approval, never self-approval.
The maximum grant lifetime is one hour; approvals cannot extend the pending request.
Owner, agent, and requester must remain posting members in a strongly fetched relay-signed roster.
A gateway requester must remain a live delegate throughout creation and retrieval.
Consumers fail closed on failed reads, expiry, deletion, replacement, or lost membership.
The same atomic acceptance burn as terminal attachment makes each approval single-use.
Consumers require the relay's [atomic first-claim receipt](../sandbox-attachment-claims.md) before starting work.

Only metadata is stored in plaintext: the approval, acceptance/end status, byte count, and optional failure category.
Failure categories are `scan`, `budget`, `archive`, `upload`, or `authority`.
Scanner output, file names, credentials, and file contents must never enter audit failures.
The agent signs an encrypted `:ready` result after confirming the ciphertext upload.
Its NIP-44 content contains version 1, approval ID, expiry, ciphertext SHA-256, exact ciphertext size, random blob key, and nonce.
Its expiration tag matches the approval expiry.
The requester verifies the agent signature, recipient, scope, approval ID, expiry, size, and ciphertext hash before decrypting.

Upload recovery uses separate agent-signed `snapshot:<session>:upload` and `snapshot:<session>:upload-outcome` coordinates.
The upload intent records the exact acceptance event, owner approval, ciphertext SHA-256, size, and approval expiry before any PUT.
It contains no blob key or nonce and has no NIP-40 expiration.
The outcome records `uploaded` or `unconfirmed` without replacing that intent.
An unconfirmed operation can have committed ciphertext; consumers must not report remote erasure or retry the PUT automatically.
The SDK validates historical upload metadata independently of current access.
The typed `snapshot::End` parser validates the exact agent, approval, scope, byte count, and optional failure category.
Completion records can arrive after approval expiry and never authorize retrieval.

The current media API has no deletion or expiry operation.
Existing upload size, rate, and concurrency limits do not provide a retained-byte quota or a retention deadline.
Ciphertext can remain indefinitely under the existing media retention policy.
Operators can use durable upload metadata to identify retained or ambiguous objects after sandbox loss.
Receipt expiry prevents new protocol transfers; it does not erase media or copies already downloaded.

Blob bytes start with `BUZZSNAP1` followed by a NUL, then XChaCha20-Poly1305 ciphertext and authentication tag.
Every blob uses a fresh random 32-byte key and 24-byte nonce.
Associated data is the canonical JSON tuple of approval ID, scope, and requester.
The key and nonce appear only inside the recipient-encrypted receipt.
The ciphertext hash names the blob on the same community's Blossom server; a receipt never supplies an arbitrary download URL.
The ciphertext cap is 100 MiB, including its prefix and tag, matching the default generic-file upload limit.
Grant expiry prevents new retrieval through this protocol; it cannot revoke bytes already copied by an authorized requester.

The archive consumer must preserve self-contained `git bundle --all HEAD` history plus the working-tree copy.
The user explicitly chose full locally available scanned history: exclusions apply to copied working-tree files, while historical versioned content remains in the bundle and must be fully scanned.
Never route snapshot creation through checkpoint quarantine or an unscanned artifact.
Scan the exact staged bundle objects and copied files, with immutable scanner rules and no repository/global ignore or replacement-ref bypass.
No forge credentials or network clone may be required to restore the bundle.
Shallow repositories carry the checkpoint format's explicit boundary metadata; consumers must retain it and validate missing parents against those boundaries, without implicit unshallow or fetch.
Unborn HEAD snapshots must be explicitly refused until a separate supported format exists.

The SDK preflight follows [Git's pack specification](https://git-scm.com/docs/gitformat-pack) and [bundle specification](https://git-scm.com/docs/gitformat-bundle).
It refuses external prerequisites, unknown capabilities, replacement refs, malformed compression, size overflow, and incomplete packs.
It bounds each declared object, delta base/result size, aggregate reconstructed size, aggregate inflated instruction bytes, object count, input bytes, and preflight time.
The limits are 256 MiB expanded/source bytes, 200,000 Git objects, 50,000 source files/references, and 30 seconds of preflight work.
Consumers may tighten the expanded-byte budget.
Preflight does not replace Git integrity checks: consumers must validate the imported object graph and inventory uncompressed sizes before checkout.
Extraction must reject unsafe paths/types, bound tar expansion and Git subprocesses, check actual free capacity for all staging/import/copy work, and publish a fresh read-only destination atomically.
Existing destinations must never be overwritten.
