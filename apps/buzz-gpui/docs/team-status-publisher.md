# Owner team status publication

Open a conversation thread and select **Share team status for this thread**.
Select an owned agent, a fixed status, and an optional public caption.
Select **Preview exact publication**, check the originating tuple, and confirm publication.
The confirmation also asserts that this is your only owner publisher across devices.

The preview shows the exact label, subject key, canonical community UUID, channel UUID, and root event ID.
The panel cannot select another recipient channel.
All authorized channel readers, including future members, can read the status.
Received information cannot be recalled.

Each transition is manual and requires another preview and confirmation.
Accepted statuses expire after 15 minutes; this layer does not refresh them automatically.
The publisher never reads observer frames, transcripts, or runner activity.
The owner key stays in the native identity service.

## Delivery and recovery

The owner panel shows enabled addresses, pending writes or clears, and delivery errors.
Select **Refresh delivery status** to read the current journal state.
Select **Stop all team status sharing** to stop labels and save clear intents.
This control remains available during preview, delivery, and errors.

The SQLite viewer preference journal survives community cache purges.
Each owner retains at most 32 addresses, including ordering watermarks.
Reaching that limit produces a visible error; clearing does not discard watermarks.
Each owner has one shared publisher across community sessions.
All sends serialize through one process-wide gate.
The native app allows one process per profile; separate profiles and devices require owner coordination.
Do not enable another profile or device with the same owner identity.

The worker attempts one pending address every six seconds.
Failed addresses rotate so they cannot starve other clears.
Before each send, it checks current ownership, membership, originating community, channel, and root.
An inaccessible root or revoked membership leaves a visible pending clear until relay authorization permits it.
Expiry bounds display freshness during that delay.

Every attempted timestamp is saved before signing.
The worker reads the relay watermark and uses a strictly newer timestamp.
Expiry is 900 seconds after that final timestamp.
Retries sign new events; they never replay expired signed clears.
An opt-out during an in-flight send keeps a newer clear pending.

Restart disables sharing and converts saved active addresses into pending clears.
Reopen the original community to retry them.
No journal data is sent through another community.
Corrupt journals fail closed and require operator recovery; they are never silently reset.

## Human smoke journey

Use a native build and a relay containing the protocol layer from PR #217.
Use one owner, an owned agent, and a stream or forum channel containing both members.

1. Open a live thread. Verify that sharing defaults off.
2. Select **Share team status for this thread**.
3. Choose `working` and enter a public caption.
4. Preview. Verify the exact label, subject, community, channel, and root.
5. Cancel. Verify that no kind `30623` event was sent.
6. Preview again and confirm publication. Refresh delivery until pending reaches zero.
7. Inspect the accepted event. Verify exactly six routing tags and the owner's signature.
8. Repeat with `waiting`, `blocked`, and `done`. Verify the originating tuple stays unchanged.
9. Disconnect the relay. Queue another transition and stop sharing.
10. Verify zero enabled addresses and visible pending clears.
11. Restart the app, reconnect, and reopen the original community.
12. Verify that clears reach every active or uncertain address and pending reaches zero.
13. Repeat after leaving the app offline for longer than 15 minutes.
14. Verify newer clear timestamps and expiry values, with no restored old label.
15. Try a DM, reply ID, missing root, non-owned agent, and revoked membership.
16. Verify visible rejection without any broader-channel fallback.
17. Use Tab, Enter, and Space for preview, cancel, publish, and stop.
18. Check the caption field and action labels with the platform screen reader.

Record the native platform, community, accepted event IDs, and results before review readiness.
Agent checks do not replace human confirmation.

## Automated checks

Run from the repository root after activating Hermit:

```sh
cargo test --manifest-path apps/buzz-gpui/Cargo.toml -p buzz-relay-client -p buzz-identity
cargo test --manifest-path apps/buzz-gpui/Cargo.toml -p buzz-gpui --no-default-features
cargo clippy --manifest-path apps/buzz-gpui/Cargo.toml -p buzz-relay-client -p buzz-identity --all-targets -- -D warnings
cargo fmt --manifest-path apps/buzz-gpui/Cargo.toml --all -- --check
```

The loopback session test uses real session operations, signing, HTTP transport, and SQLite recovery.
The local HTTP fixture replaces only relay responses and delivery failures.
The journal journey also covers uncertain in-flight writes and retries after clear expiry.
These tests do not validate rendered native widgets, platform accessibility, or deployed relay authorization.

Native builds require platform development libraries, including GStreamer and xkbcommon on Linux.
Missing libraries block native compilation and UI journeys; report those checks as blocked.

The change exceeds the 1,000-line target because the transport recovery fixture exercises the actual session boundary.
The publisher, its native workflow, and recovery tests form one dependent layer.
Native widget observation also requires the GPUI test-support dependency graph.

The native widget journey drives preview through the real offline session and verifies keyboard access to Stop.
Its execution remains blocked by missing native libraries in the worker image.
The loopback journey validates successful publication and recovery independently of native rendering.
