# Contained snapshot producer worker

The native `buzz-acp sandbox-snapshot-worker` subcommand runs one bounded producer under a dedicated Linux subreaper.
The worker receives no relay or provider credentials.
The ACP snapshot consumer follows in a dependent layer.

The internal argument order is workspace, fresh output path, Unix expiry, work-byte budget, and trusted helper path.
Production consumers must fix the helper to `/usr/local/bin/buzz-agent-snapshot`.
A relay request must never select the helper, workspace, output path, or budget.

The worker supplies `--workspace`, `--output`, and `--work-bytes` to that helper.
It clears the environment and retains only `PATH`, `HOME`, `LANG`, and `LC_ALL`.
It sets `TMPDIR` to the private output directory and supplies empty standard input.
The helper must preserve that scratch boundary.

The producer report is bounded to 4 KiB; its error report is bounded to 32 bytes.
The worker accepts only version 1, `scanned:true`, at most 64 repositories, and a matching private regular output file.
The ciphertext consumer enforces the separate 100 MiB artifact limit.
Failure metadata contains only `scan`, `budget`, `archive`, or `authority`.
The worker parses the packaged helper's fixed JSON error category; it never forwards diagnostics.

EOF, termination, parent loss, expiry, or the 180-second work deadline stops the producer.
The worker kills and reaps adopted descendants before publishing its final receipt.
Exit 0 confirms success; exit 1 confirms a failed operation after containment cleanup.
Exit 2 means containment could not be confirmed and requires the parent harness to terminate.

Run `cargo test -p buzz-acp --test sandbox_snapshot_worker` on Linux.
The tests execute the actual binary and cover detached double-fork children, parent loss, sibling preservation, report limits, private files, and fixed failure categories.
