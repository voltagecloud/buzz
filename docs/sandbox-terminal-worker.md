# Contained sandbox terminal worker

This layer provides a local PTY process boundary for the relay terminal runtime.
It does not subscribe to relay events or accept an attach grant. The runtime
consumer must verify the owner-approved protocol from [NIP-TA](nips/NIP-TA.md)
before starting a worker, and must stop it when that authority ends.

The harness starts a separate `buzz-acp sandbox-terminal-worker` process with
the workspace and grant expiry. The worker establishes and verifies a Linux
child subreaper before opening a PTY. Reparented jobs, including `setsid` and
double-fork children, therefore remain its descendants. Harness and agent
siblings are outside that process tree.

The implementation uses the safe async API from
[pty-process 0.5.3](https://docs.rs/pty-process/0.5.3/pty_process/).
It retains the workspace's existing Tokio and Rustix versions. There is no
Python helper, image copy, tunnel or additional network listener.

## Local protocol and limits

Input is newline-delimited JSON using the NIP-TA `open`, `data`, `resize` and
`close` packet shapes. Output is `ready`, a `packet` containing bounded data,
or `ended` with a failure flag. The first input must open the PTY. The runtime
must wait for `ready` before acknowledging the remote client's open packet.

| Resource | Bound |
| --- | --- |
| JSON input or output line | 32 KiB |
| Decoded data packet | 16 KiB |
| Terminal dimensions | 1 through 1000 rows and columns |
| Initial open and each I/O operation | 5 seconds |
| Grant lifetime | At most one hour, enforced with a monotonic deadline |
| Idle without input or resize | Five minutes; empty heartbeats do not reset it |
| Descendants | 256 |
| Process scan | 4096 processes; 4096 bytes per stat record |
| Complete process cleanup | Five seconds |
| Final output after natural shell exit | 64 KiB and one five-second drain deadline |
| Tokio shutdown after a cancelled stdin read | 100 milliseconds |

The shell is `/bin/sh -i`, running in the supplied workspace as the agent user.
Its environment contains only PATH, HOME, LANG, LC_ALL, TMPDIR and TERM. This
removes ambient relay and model credentials; it does not isolate files or
privileges already available to the sandbox user. Per-session owner approval
must convey that shell access includes those sandbox privileges.

## Shutdown contract

EOF, close, termination signals, idle, expiry and I/O failure all lead to
process cleanup. On natural shell exit, the worker first stops background jobs
that could retain the slave, then drains buffered output before reporting the
end. Exceeding the drain byte or time bound reports failure. The worker stops and kills its descendants, then reaps adopted
children until the tree is empty. A pidfd plus process start-time verification
prevents PID reuse from directing a signal at an unrelated process.

Exit 0 means the operation ended with confirmed containment. Exit 1 means an
operation or receipt failed after containment was confirmed. Exit 2 means
setup or complete containment could not be confirmed. The runtime consumer
must treat exit 2 as fatal and terminate the harness so the container lifecycle
can remove remaining processes. It must never report an ordinary successful
close for that outcome.

Linux subreaper and pidfd support are required. Other platforms refuse this
worker mode. The existing Buzz app flows do not invoke it.

## Verification

```sh
cargo test --locked -p buzz-acp --test sandbox_terminal_worker
cargo test --locked -p buzz-acp --lib terminal_process_selection
```

The first suite runs the production binary with a real PTY and checks terminal
resize, environment filtering, EOF, expiry, malformed input, final output after
shell exit with a background holder, and a detached
double-fork child. It also verifies that a sibling process survives cleanup.
The second checks the production process-selection boundary and descendant cap
without sending signals to host processes.
