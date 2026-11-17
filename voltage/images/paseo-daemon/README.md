# Paseo personal daemon

One developer's headless Paseo daemon stays available while their laptop is closed.
The image uses unmodified Paseo 0.15.3 with Claude Code 2.1.325 and Codex CLI 0.169.0 from the shared runtime lockfile.
It adds the Buzz and via CLIs, a configuration wrapper, read-only Git helpers, a turn recorder, and a gateway profile plugin.
Each enrolled subscription enables its personal assistant independently. Both providers can remain available together.

| Credential | Paseo provider |
| --- | --- |
| `CLAUDE_CODE_OAUTH_TOKEN` | Personal assistant (Claude) |
| `CODEX_ACCESS_TOKEN` | Personal assistant (ChatGPT) |

To switch subscriptions, start a new conversation and select the other personal assistant in the provider picker.
Native conversation histories belong to their respective harnesses. Both assistants use the same owner-scoped relay memory and Buzz delegation tools.
Hosted deployments retain Claude and Codex history in the persistent home; their backup policy covers both harnesses.
An existing enrolled Codex access token is reused. An OpenAI Platform API key does not enable this subscription provider.
To add a token, run `buzz paseo setup --update`, enter the Codex access token, and leave existing credentials blank.
After a credential change, an operator must restart the daemon to reload its Secret.

### Codex sandbox prerequisite

Codex runs with read-only filesystem access and disabled command networking.
Choose **Auto-review** to let Codex review eligible approvals, or **Default Permissions** to review them yourself.
Shell escalation and permission expansion are disabled in both modes.
For **Full Access** and implementation work, hand off to a Codex agent in an isolated Buzz sandbox.
Image-owned requirements prevent full-access mode and deny reads of credential files and process environments.
Codex shell processes receive no gateway, Git, model, or pairing credentials.
Its scoped `buzz_read` tool provides authenticated reads and code fetches through the existing CLIs.
The `buzz_write` tool submits each message, workflow change, or relay-memory write for review, with exact text supplied inline. Auto-review can decide eligible tool prompts.

Codex uses bubblewrap, which needs unprivileged user namespaces. The default container syscall filter blocks those operations.
The image supplies a capability-free OCI seccomp profile derived from [Moby's pinned default](https://github.com/moby/profiles/blob/f862455628f78f84c937e967e4a03902fb1dee11/seccomp/default.json).
The build verifies its checksum, evaluates capability and architecture conditions, and adds only bubblewrap's namespace operations.
The container must remain non-root, with all capabilities dropped and privilege escalation disabled.
Mount operations remain unavailable in the host user namespace.

Before starting hosted daemons, install `/etc/paseo-buzz/seccomp.json` as kubelet's `paseo-codex.json` Localhost profile on each daemon node.
Use an operator-controlled node installer for new autoscaler nodes; never grant the assistant a host filesystem mount.
Keep the namespace's restricted Pod policy. Select the Localhost profile only for the daemon container; backup and restore helpers retain RuntimeDefault.
The bundled profile requires Linux 4.8 or newer and supports the image's amd64 and arm64 architectures.

```mermaid
flowchart LR
  Client[Paseo client] <-->|WSS encrypted frames| Relay[Paseo relay /ws]
  Relay <--> Daemon[Personal daemon: Claude or ChatGPT and code reads]
  GitHub[Token-permitted GitHub repositories] -->|read-only partial clone and fetch| Daemon
  Daemon <-->|WSS signed messages| Buzz[Buzz relay]
  Buzz <--> Sandbox[Sandbox: writes, builds, tests and pushes]
```

The daemon reads code, reviews changes, and sends briefs or suggested diffs as ordinary Buzz messages.
The sandbox decides which suggestions to apply and pushes a branch for review.
User-visible changes should include a sandbox-produced walkthrough video in the thread.
The daemon does not run the site or expose a sandbox development server.

See [workspace records](workspace-records.md) for owner-managed Paseo workspace commands and the shared reader contract.

## Identity and access

The manager stores pairing credentials and the distinct gateway key in `paseo-daemon-<developer>` in namespace `buzz-paseo`.
The owner's private key never enters the pod.
The pod has no IAM role, Kubernetes token, or sandbox credentials.
The owner runs `buzz paseo setup` to enroll, create the daemon, and grant the gateway.
Grant and membership are separate: admit the gateway to the workspace and each required channel, including its recording channel.
A gateway grant conveys no delegated access to the owner's DMs, profile, or administrative privileges.

The `buzz-profile` plugin gives the gateway key its own Buzz profile name, so its posts show a name instead of a pubkey prefix.
At startup it reads the display name of `BUZZ_PASEO_OWNER` and publishes `<owner name> (Paseo)`, for example `Taylor (Paseo)`.
The about line names the owner's pubkey. The profile is the gateway's own kind 0 and claims no owner authority.
If the gateway already has that name, the plugin writes nothing; a renamed owner renames the gateway at the next start.
Until the gateway is a relay member, or while the owner has no name, it retries with exponential backoff (1 minute up to 1 hour) for 24 hours.
Without `BUZZ_PASEO_OWNER`, the plugin is disabled.

See [the enrollment runbook](../../remote-agents.md) for the existing commands and pairing password retrieval.

For code reads, supply a fine-grained GitHub token with `contents: read` and `pull_requests: read`, with no write permissions.
It must cover the same repository set the Buzz agents can read.
Provision a separate token; never reuse the agents' existing write-capable token.
Secret data `github_read_token` maps explicitly to `GITHUB_READ_TOKEN` through Terraform's `secretKeyRef`.
The manager preserves this field; it never copies the sandbox profile's broader GitHub credential.
The entrypoint also exports this read token as `GH_TOKEN` for Paseo repository discovery.
Search requires an upstream Paseo release with token-based repository listing; the current 0.15.3 pin does not include it.
The Git credential helper still requires a repository path.
Configure `GITHUB_READ_TOKEN_REPOSITORIES` separately as a JSON array of repository names, such as `["your-org/private-repo"]`.
This nonsecret list is a startup prefetch manifest: these repos get cloned and registered as projects.
Everything else the token permits is still readable on demand; the manifest is not an access gate.
It permits at most 64 unique `owner/repository` entries and 16,384 encoded bytes.
Names match case-insensitively; URLs, traversal, and extra path segments are refused.
The credential helper requires HTTPS `github.com` and a canonical `owner/repository` path; Git passes paths through `credential.useHttpPath=true`.
A missing or empty manifest does not restrict credential delivery.
Without a token, an empty manifest permits anonymous public clone/fetch; a non-empty manifest refuses startup.
Private repositories still require a valid token with matching GitHub permissions.
The wrapper refuses broad `gho_` or classic `ghp_` tokens.
Token syntax cannot prove its permissions: the developer must verify the repository and read-only scopes in GitHub.
If the token permits writes, the daemon can push with that token; the runtime does not override its GitHub authority.

**The GitHub token's scope is the security boundary.**
Provider tool restrictions reduce mistakes; they are not a security boundary once a provider has a shell.
The container uses a read-only root filesystem, an unprivileged UID, dropped capabilities, and a one-CPU limit.
It carries no package managers, compilers, build/test tools, or scanners.
Node remains necessary for the upstream daemon and plugin runtime.
Analysis tools include jq, Python 3, sqlite3, xxd, file, tree, unzip, zstd, less, fd, column, and hexdump.
`fd` links to Debian's `fdfind`. Debian's [yq package](https://packages.debian.org/bookworm/yq) uses jq filters through Python; it is not mikefarah/yq.
Python supports standard-library analysis; the image adds no pip or user-installed Python packages.
Never install packages or execute repository scripts here.

## via client and enrollment prerequisite

The image packages via 0.6.1 for Linux amd64 and arm64, built statically with musl from SHA-256-pinned source at `87d1301158d9c1ac585110869f8bb993b15270da`.
It was the latest published release when checked on 2026-11-14.
The published Linux binaries require glibc 2.39, which the daemon runtime does not provide.
The source download and locked build run in a separate Alpine builder; no installer or package manager is added to the runtime.

`via` selects `$HOME/.config/paseo/via.toml`, separate from the Sandbox's enrolled profile.
`via version` and `via --help` work before enrollment.
`via capabilities` fails when that daemon profile is absent; it never falls back to the Sandbox profile or via's default configuration.
The wrapper does not load a vault token, export service credentials, or enroll services.
It selects configuration and is not an authorization boundary.

**This image change installs the client only. It does not enable service reads.**
Today's GPUI/manager enrollment provisions the worker Sandbox profile, not this daemon file.
The manager's daemon Secret allowlist and Terraform mounts need separate provisioning work before the daemon can use services.
Do not copy the worker's profile, `op-token`, or `OP_SERVICE_ACCOUNT_TOKEN` into the daemon.
No enrollment, manager, or Terraform changes are included here.

### Proposed enrollment contract for tee8z

Enroll one daemon-specific profile per developer and mount it read-only at `$HOME/.config/paseo/via.toml`, readable by UID 1000.
The policy is **all available read capabilities**, excluding mutations and operations that return credential material.
Enforce that rule through the service provider or a credential-isolating broker; assistant tool permissions cannot enforce it.
Keep service credentials outside the shell-enabled daemon and grant its caller only the approved read authority.
Do not activate a service until its provider enforces the complete rule.

| Capability | Enrollment requirement |
| --- | --- |
| Read-only service data | Permit every operation the provider classifies as a credential-safe read. |
| Writes, deploys, cluster changes, package installs | Deny at the provider, regardless of assistant approval. |
| Secret values, token-bearing environments, credential files | Deny, including reads that return them inside larger responses. |
| GraphQL reads, including Linear | Distinguish queries from mutations at the provider; HTTP POST alone does not classify the operation. |
| CI logs, application logs, artifacts, rendered manifests | Treat as potentially credential-bearing; expose only through a provider that enforces the credential exclusion. |
| Git credentials | Retain the separate `GITHUB_READ_TOKEN_REPOSITORIES` boundary from main; this change does not alter Git behavior. |

The published via 0.6.1 profile cannot enforce this contract by itself.
Its REST capabilities accept arbitrary HTTP methods and have no method/path policy.
[Via's newer broker design](https://github.com/tee8z/via/blob/ca7edf707bb73bfe93887b33a082b7463c1432f9/docs/broker-mode.md) isolates credentials and adds method/path rules.
It still does not inspect GraphQL bodies or identify arbitrary credentials in API responses.
A broker-capable release and service-side enforcement are enrollment prerequisites; no unsupported policy fields are added to the v0.6.1 config.
Tee8z must provision that enforcement and daemon configuration separately, then validate each enabled service before rollout.

Raw CI logs can echo secrets.
[GitHub documents that automatic masking is not guaranteed](https://docs.github.com/en/actions/reference/security/secure-use), including when secret values are transformed.
Via's exact-value redaction also does not establish that a log is credential-free.
This image adds no GitHub Actions capability and enables no new CI-log reads.

After provisioning a compliant profile, run `via capabilities` in the daemon and exercise one permitted read per service.
Verify denied writes and credential-returning requests against isolated fixtures, without production probes or real secrets.
Assistant shell calls retain the existing Paseo approval behavior; this change adds no via tool allowlist.

## Read-only code workflow

```bash
paseo-code clone voltagecloud/buzz
paseo-code fetch voltagecloud/buzz review/branch
paseo-code diff voltagecloud/buzz origin/main HEAD
paseo-code show voltagecloud/buzz origin/review/branch
paseo-code view voltagecloud/buzz review/branch
```

Clone and fetch always request `--filter=blob:none` and disable tag fetching.
Clone materializes the initial checkout; Git fetches file contents on demand.
Checkouts live under `$HOME/code/<owner>/<repository>` and disappear with the pod.
Open this directory in Paseo's file and diff views.

`paseo-workspaces` reads `buzz paseo workspace list --owner <BUZZ_PASEO_OWNER>` every 60 seconds.
The CLI validates owner signatures, workspace markers, channel policy, directory names, ordering, and caps.
The daemon does not parse relay events.
Each accepted workspace becomes a non-Git project at `$HOME/workspaces/<directory>`.
Literal slug directories and hashed names come directly from the shared Rust implementation.
Rootless workspaces need no repository.

The daemon registers `~/buzz` and every workspace root before manifest cloning.
Workspace directories stay empty; their `cwd` selects the session's posting channel.
`buzz-source` entries are prompt context, not source views or prefetch requests.
The assistant clones a needed repository with `paseo-code clone owner/repository` and reads under `~/code/<owner>/<repository>`.
The existing one-time manifest prefetch and `paseo-code scope` behavior remain in place.
Repositories outside this startup prefetch manifest remain readable on demand when the token permits them.
Existing checkouts are not fetched or reset automatically.

Upstream `resolveScopedPath` restricts the file explorer UI, not the daemon's direct Read, Glob, Grep, diff, or show operations.
External directory links fail that UI boundary, and Grep does not follow source symlinks by default.
Workspace worktrees would add checkouts solely for a file-tree affordance; the daemon's standing instructions forbid repository writes and commits.
The owner therefore removed source materialization from design section 4.4.
Two empty workspace roots still register as separate non-Git projects.
Upstream displays a non-Git root's last path segment literally: `payments-review` stays `payments-review`.

A failed workspace or manifest read changes nothing.
Overlapping sync ticks are skipped; failed operations retry on the next tick.
Removed or invalid workspaces retain their project registration and directory.
The daemon retains at most 32 unused retired directories, plus directories used by live agents.
Cleanup only removes empty retired roots and rereads live agent records before each removal.
Unreadable or incomplete agent records disable cleanup.
The daemon never removes shared clones under `$HOME/code`, including on-demand clones; they remain until pod replacement.
Retirement timestamps and active directory names are stored under `$PASEO_HOME/buzz-workspaces.json`.
All state is pod-local and rebuilt from the relay after replacement.

Fetch with a branch updates `origin/<branch>` and detaches the checkout at that fetched branch.
It refuses to overwrite local changes and creates no worktrees.
The developer compares the current branch against `origin/main` in Paseo's diff view.
Fetch without a branch refreshes remote refs without changing the checkout.
View fetches one branch and puts it in a detached worktree at `$HOME/code/<owner>/<repository>@<branch>`, beside the checkout.
It clones the repository first when needed and never moves the main checkout.
A later view of the same branch moves the existing worktree to the fetched head and discards changes to tracked files there.
View accepts repositories outside the startup prefetch manifest and refuses a branch whose view would nest inside another view.
The PA runs clone, fetch, diff, and show without approval.
`view` is not on the PA's allowlist, so the PA must get the developer's approval for each `paseo-code view`; the bridge, a provider process, is its intended caller.
The bridge passes `--inherit-process-group` so Git stays inside its bounded process group.
Deploy this helper before the file-view bridge; older helpers reject that flag before starting Git.
Claude's Read, Glob, and Grep tools remain available; Write and Edit tools are disabled.
The helper has only clone, fetch, diff, show, and view operations.
The credential helper sends credentials only to canonical HTTPS `github.com` repository paths, outside argv, URLs, and repository config.
Git hooks, submodule recursion, automatic maintenance, and non-HTTPS transports are disabled by generated configuration.
Git operations have a three-minute process-group deadline.

The default assistant prompt asks sandbox agents to apply changes, build, test, scan, and push.
Native terminals and workspace script tools are disabled.

## Providers and configuration

`BUZZ_PASEO_AGENTS` adds at most 64 ACP providers:

```json
[{"id":"brokkr","label":"Brokkr","agent":"<64 hex characters>","channel":"<UUID>"}]
```

Each provider runs `buzz acp-bridge --agent <key> --owner <owner key>`, with an optional legacy `--channel <UUID>`.
A session’s workspace selects its posting channel.
The assistant prompt lists current agents and workspaces and directs `create_agent` to use a `workspaceId`.
The agent watcher refreshes both lists without a restart and remains the only `config.json` writer.
Each provider declares one preselected model, so the model picker does not ask for a choice; the bridge offers no ACP model selection.

New or changed agents need no daemon restart when Terraform mounts the daemon Secret's `BUZZ_PASEO_AGENTS` key at `/etc/paseo-buzz/agents/agents.json`.
`paseo-agents-watch` checks that file every 30 seconds.
When the list changes, it validates the list with the same builder as startup (`providers.mjs`), replaces only the Buzz providers in `config.json`, and runs `paseo reload`.
Kubernetes refreshes the mounted file about a minute after the manager updates the Secret, so a new agent appears within about two minutes.
An invalid list is logged and the current providers stay; a failed reload is retried on the next check.
Without the mount, the watcher does nothing and agents change only at the next restart.
The bridge receives the gateway key and relay URL, without model, pairing, or GitHub credentials.
The bridge in #149 supplies relay-backed session list/load; #154 supplies gateway authority.
The final image must include current #149, #154, and manager #150 through the adopted core stack.
No parked feature is required.

| Variable | Contract |
| --- | --- |
| `PASEO_PASSWORD` | Required, at least 16 characters. |
| `PASEO_SERVER_ID` | Required stable `srv_` identifier. |
| `PASEO_DAEMON_KEYPAIR` | Required matching Curve25519 `{v:2,publicKeyB64,secretKeyB64}`. Written mode 0600, then removed from the environment. |
| `BUZZ_PRIVATE_KEY` | Required gateway signing key. Never the owner or sandbox key. |
| `BUZZ_PASEO_OWNER` | Owner public key; required with remote providers. Missing disables relay memory. |
| `BUZZ_PASEO_AGENTS` | Optional JSON array above. Missing or `[]` enables no remote providers. |
| `CLAUDE_CODE_OAUTH_TOKEN` | Optional Claude subscription credential. Enables the Claude personal assistant. |
| `CODEX_ACCESS_TOKEN` | Optional ChatGPT workspace Codex access token. Enables the ChatGPT personal assistant. |
| `GITHUB_READ_TOKEN` | Optional fine-grained `github_pat_...` credential. Missing permits anonymous public clone/fetch with an empty manifest; a non-empty manifest fails startup. |
| `GITHUB_READ_TOKEN_REPOSITORIES` | Nonsecret startup prefetch manifest of `owner/repository` paths cloned and registered as projects; default `[]`, at most 64 entries and 16,384 bytes. Other token-permitted repos remain readable on demand. |
| `BUZZ_RELAY_URL` | Default `wss://buzz.voltage.cloud`; WSS only. |
| `PASEO_RELAY_ENDPOINT` | Default `buzz.voltage.cloud:443`, without a scheme or path. |
| `PASEO_RELAY_PUBLIC_ENDPOINT` | Defaults to the relay endpoint and appears in pairing offers. |
| `PASEO_RELAY_ENABLED` | Must be `true`. |
| `PASEO_RELAY_USE_TLS`, `PASEO_RELAY_PUBLIC_USE_TLS` | Both must be `true`. |
| `BUZZ_PASEO_RECORD_CHANNEL` | Optional channel UUID. Missing disables recording with a startup diagnostic. |
| `BUZZ_MANAGER_PUBKEY`, `BUZZ_MANAGER_CHANNEL` | Optional manager identity and channel; set both or neither. |
| `PASEO_LISTEN` | Default `127.0.0.1:6767` for in-container tools and health checks. |
| `HOME`, `PASEO_HOME` | Defaults `/home/paseo` and `$HOME/.paseo`. |
| `PASEO_WEB_UI_ENABLED`, `PASEO_DICTATION_ENABLED` | Disabled; generated configuration also disables voice. |
| `DISABLE_AUTOUPDATER`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Defaults `1`. |

Malformed present values refuse startup with exit code 64, without printing secrets.
Without either model token, the daemon still offers its enrolled Buzz remote providers.
The obsolete host-tools switch has no effect; terminals stay disabled.
Pairing links use `https://app.paseo.sh` even when the daemon's local web UI is disabled.
Desktop, mobile, and web clients use the relay endpoint embedded in the offer.

## Conversation restore

The daemon's home is pod-local, so a new pod starts with no Paseo agents, but every remote-agent conversation is still a Buzz thread.
`paseo-agents-watch` restores each configured Buzz provider at startup, and each provider that a later reload adds.
It starts when the daemon answers `paseo project ls` with `~/buzz` registered, and does not wait for repository clones.
For each provider, `restore.mjs` does these steps:

1. It restores the provider only when `$PASEO_HOME/agents` has no record of it, active or archived. It checks before and after the listing.
2. It starts the provider's own bridge command with the provider's environment and sends ACP `initialize` and `session/list`.
3. It keeps the 20 newest sessions updated in the last 14 days, reading at most 10 pages. When a later page fails, it keeps the earlier pages.
4. It runs `paseo import --provider <id> --cwd <dir> -- <session>`, newest first, with the provider's environment.

Restore applies only to a fresh home for that provider.
A conversation started in Paseo stores the bridge's own session id, not the thread id that `session/list` reports.
So a provider with any record (a kept home after a container restart, or a conversation started in Paseo) is left alone, and restore never imports a duplicate.
A provider whose entry changes in a reload is not restored again.

The directory is the listed `cwd` when it is an existing directory under `$HOME`, outside hidden directories and reached without symlinks.
An empty workspace directory is sufficient; restore never requires its source repositories.
Otherwise it is `~/buzz`, a project registered at startup.
A conversation whose repository is not cloned yet also goes to `~/buzz`.

| Bound | Value |
| --- | --- |
| ACP request | 30 seconds |
| One provider's bridge, spawn to kill | 60 seconds |
| One `paseo import` | 60 seconds |
| Whole run | 5 minutes |
| Bridges or imports at once | 2 |
| Non-archived agents of Buzz providers, restored or started in Paseo | 40; restore imports nothing above it |
| Bridge stdout | 4 MiB, at most 1 MiB per line |
| Daemon readiness wait | 60 polls, 2 seconds apart |
| Attempts per provider | 3: at startup, after 5 minutes, after 10 more minutes |

Each bridge and import runs in its own process group, which is killed at completion or at its deadline.
Each live Paseo agent can keep its own idle bridge process (about 7 MiB RSS each, measured without a relay connection).
The cap counts the live agents of all Buzz providers, so restored agents stay at most 40, about 280 MiB of bridges, over any number of runs.
A failure is logged to stderr and never blocks or stops the daemon.
When the daemon is not ready, a listing fails, or an import fails or misses the run deadline, the restorer tries that provider again after the backoff.
A retry after a failed import imports only the failed sessions and skips any session that Paseo has meanwhile.
After the third failed attempt, it logs `gave up restoring <provider>` and stops.
The developer then imports the remaining conversations by hand: choose **Import session → Show all**, then select the remote provider and conversation.
The same sheet opens older conversations and those above the cap.
Paseo's [upstream import sheet](https://github.com/getpaseo/paseo/blob/v0.15.3/packages/app/src/components/import-session-sheet.tsx) supports this global view without a fork.
`node restore.mjs [provider…]` runs one attempt without retries, and exits 1 when something failed.
The image test imports a listed session through the real daemon and checks that a second run leaves the provider alone.
Visual client interaction remains part of the human acceptance test.

## State and replacement

| State | Store | New pod behavior |
| --- | --- | --- |
| Pairing key, server ID, password, gateway key, narrow GitHub token | Per-developer Kubernetes Secret | Regenerate files from the same identity. |
| Remote-agent conversations | Buzz relay | Startup imports each provider's 20 newest conversations from the last 14 days; older ones through **Import session**. |
| Accepted assistant transcript messages | Buzz recording channel | New assistant session reads the latest 20 records for context. |
| Assistant memory (rules, preferences, lessons) | NIP-AE engrams on the Buzz relay, written by the gateway key for the owner | New assistant session reads `core` and follows its `[[mem/…]]` links. |
| Unsent transcript intents and dead letters | Pod-local `$PASEO_HOME/buzz-recording` | Lost on pod replacement. |
| Native Claude and Codex sessions | Daemon home | Continue when the hosting environment retains the home; restore supported history from its backup on volume loss. |
| Paseo's agent list and project registry | Pod-local `$PASEO_HOME` | Rebuilt at startup: prefetch manifest repositories and `~/buzz` as projects, restored remote-agent conversations as agents. |
| Branch views | Pod-local `$HOME/code/<owner>/<repository>@<branch>` | `paseo-code view` again on demand. |
| Git checkout | Pod-local `$HOME/code` | Partial clone again on demand. |
| Paseo config and instructions | Generated from env and this image | Regenerate at startup. |
| Local schedules and heartbeats | Disabled | No persistence promised. |
| Approved scheduled Buzz workflows | Buzz relay | Continue without the daemon, under the narrowed #161 authority contract. |

The assistant reads recorded context as historical data, not fresh instructions.
Memory reads (`buzz mem ls`, `get`, `hash`) run without approval. A write (`set`, `patch`, `rm`) submits its exact inline text for review. Claude prompts the developer. Codex follows the selected approval mode.
Memory is tied to the gateway key: its addresses and encryption derive from it, so a new gateway key starts with empty memory. The owner can still read the old entries with `buzz mem ls --agent <old gateway pubkey>`.
Rules you paste into the assistant chat to seed memory are developer messages, which the recorder posts to the recording channel; do not paste anything you would not post there.
The developer can audit the memory with their own key: `buzz mem ls --agent <gateway pubkey>`, then `buzz mem get <slug> --agent <gateway pubkey>`.
It does not reconstruct provider session files or claim complete native-session recovery.
A Kubernetes Secret survives pod deletion; neither an `emptyDir` nor container disk does.

Local schedule and heartbeat tools are disabled for the assistant.
Any schedule created manually through a paired client remains pod-local and disappears on replacement.
For recurring tasks, the assistant proposes a Buzz workflow with a schedule trigger and literal prompt.
Workflow create, update, and trigger commands go through approval.
The assistant can list workflows and delete them on request.

Each `buzz messages send` goes through approval. Choose manual review if you want to see each post before publication.
The assistant can read files only under `~/code`; reads of `/proc`, `/sys`, `/etc`, `/run`, `~/.paseo`, `~/.claude`, `~/.config`, and Git or SSH credentials are denied.
These rules reduce what a prompt-injected assistant can leak; they are not a hard boundary, because the `buzz` CLI and the Git helper need the gateway key and the read token in the daemon's environment.
The gateway key's limited, revocable delegation and the read-only token are the boundary.

## Recorder bounds and loss

The directory plugin records every developer prompt and assistant message in one ordinary Buzz thread per native Claude or Codex session.
It uses before/after-turn hooks and projected timeline reads; complete assistant messages publish when the turn ends.
No tool calls, names, statuses, outputs, or reasoning enter the transcript.
Remote providers already write to Buzz and are excluded.

Each part carries `t=paseo:developer` or `t=paseo:assistant` and a validated `paseo` correlation tag.
Both roles are signed by the gateway key; role tags describe transcript content.
The correlation records session, message, part count, and sequence.
Replies point directly to the session root, so thread depth stays constant.
Each part receives its own wall-clock second for ordinary-client chronological ordering.
Long UTF-8 text splits into parts without truncation.

The recording channel must contain no `message_posted` workflows.
Do not configure an agent to consume every message in this channel.
Copied mentions, Nostr URIs, and emoji shortcodes remain literal text.
Transcript publication emits no `p`, wake, broadcast, media, or emoji tags.
Role tags do not grant authority or trigger a special relay path.

Known environment secrets and credential-shaped values omit the whole affected field.
Public 64-hex event IDs and pubkeys remain usable; known secret-key values are still omitted.
This reduces accidental disclosure and cannot classify every possible secret.
Use a dedicated membership-gated recording channel.

The recorder persists an intent before reading the timeline.
Before publication, it freezes the text, timestamp, thread metadata, and stable message identity.
Ambiguous acknowledgement retries produce the same signed event ID.
Only explicit `accepted: true` advances the journal.
A persisted session cursor prevents before/after hooks from duplicating already recorded entries.
Session cursors are bounded to 256 sessions and remain pod-local. At the limit, the least recently used cursor of a session with no pending job is evicted; a later turn of that session starts a new recording thread.

Each flush attempts eight session heads, preserving order within each session.
Failures rotate across sessions and keep retrying, at most once a minute, for 24 hours, so relay rollouts and failovers lose nothing.
A job that still fails after that becomes `<id>.dead.json`. Dead letters do not count toward the 256-job cap; the newest 32 are kept and older ones are deleted. A recording channel the gateway cannot post to shows up as repeated failures in the daemon log.
The spool reserves seven MiB for journals and one MiB for atomic replacement, including session cursors.
Each turn job is limited to one MiB including frozen metadata; overflow fails explicitly and never truncates text.
At most 256 capture identities wait in memory.
Timeline requests have five-second deadlines and scan at most 1,000 projected entries.
Publisher process groups have 15-second deadlines and bounded output.
Unsupported old summary jobs remain as dead letters rather than being published as transcripts.

SIGTERM stops admission and starts a 1.5-second final flush budget.
Each remaining active job gets at most one shutdown attempt while that budget remains.
Upstream signals plugin shutdown after two seconds and kills it after four seconds.
The plugin budget finishes before that first force signal.
The pod uses a 60-second termination grace period.
Timeouts retain unacknowledged files, but pod deletion can lose the bounded local spool.
SIGKILL and node loss have no flush opportunity.
Messages already accepted by the relay survive.
A dead-lettered turn can leave a gap; inspect it rather than assuming a complete transcript.

## Build and validation

```bash
docker build -f voltage/images/paseo-daemon/Dockerfile -t paseo-daemon:local .
bash voltage/images/paseo-daemon/test-image.sh paseo-daemon:local
node voltage/images/paseo-daemon/test-recorder.mjs
node voltage/images/paseo-daemon/test-code.mjs
node voltage/images/paseo-daemon/test-restore.mjs
```

The image contract uses synthetic credentials, no network, a read-only root, UID 1000, and dropped capabilities.
On AppArmor-enabled Docker hosts, the test loads a temporary, container-specific policy that permits bubblewrap's namespace operations and removes it during cleanup.
This requires `apparmor_parser` and permission to load a policy on the Docker host, through passwordless `sudo` when necessary.
The host's default policy stays unchanged; the test retains seccomp and all container restrictions.
It checks identity, TLS refusal, Claude-only selection, tool policy, runtime inventory, recorder loading, and bounded retries.
Separate runtime evidence covers same-Secret replacement and real relay session list/load.
A developer must still perform the staging acceptance steps in the shared runbook.

### Resource observations

The 2026-11-10 owning-layer measurements used one CPU, a 2 GiB observation limit, disk-backed home, and two live bridge processes.
The bridges imported actual relay conversations through upstream Paseo discovery and import APIs.
Only the local TLS provider fixture used an additional native-root feature; production bridge source was unchanged.
The normal image CLI and Git helper performed transcript publication and public repository reads.
The clone candidate was `sha256:236157bd565b321ca1bc931247f5100850499bb184dca90b46697276b0cdfa4e`.
Its Git helper SHA-256 was `89e173a267f9e4ca77e4cc195ef838fc0a199d2c60ab7cd2dfd705eb8a8576e9`.
The earlier confirmed Claude turn used candidate `sha256:4c2a87d65d2a9c449e16491f1bfcc9c59face5969e7e21a90e08f1234b02c7d7`.
Both used unmodified Paseo 0.15.3 and the private native-root ACP provider built from production source `6ae1e67868891ec5a9b207a2b26cb252a911b5f1`.

| Operation | Wall time | Container memory peak | Home disk after operation |
| --- | --- | --- | --- |
| Confirmed Claude turn reading relay context | 6.472 seconds | 734.6 MiB | 900 KiB |
| Anonymous `voltagecloud/buzz` partial clone | 10.865 seconds | 722.1 MiB | 275,700 KiB |
| `lightningnetwork/lnd` partial clone, retaining Buzz checkout | 8.829 seconds | 775.5 MiB | 348,476 KiB |

Both clones used `blob:none` without shallow history.
The lnd clone had a synthetic token present, scoped through configuration to private repositories elsewhere.
Actual TLS Git traces showed no outgoing Authorization header for either public repository.
Neither clone reached its memory limit or generated an OOM event.
The Claude turn and clones were separate observations; no additional model turn ran after the credential-scope fix.
Authenticated private reads remain unproven until the developer supplies the approved token.
Taylor explicitly accepted that limitation without blocking review readiness.

Recommended starting resources are 250m CPU and 768 MiB memory requests, with one CPU and 2 GiB memory limits.
Allow 2 GiB for home and 256 MiB for temporary files, with 1 GiB ephemeral request and 3 GiB ephemeral limit.
These allowances include headroom for model activity alongside checkout growth; they do not promise unlimited branch caches.

The image workflow publishes staging ECR only after merge; its summary gives the manifest digest.
Keep Terraform's image value empty until the manager and preceding core PRs are deployed in the authorized rollout.
The final aggregate package still requires a rebuild from the adopted core stack and its applicable validation gates.
No apply, sync, deployment, or registry push is part of this change.

## Workspace rollout

This image requires the workspace CLI and bridge routing from PRs 3 and 4 in the final image build.
Deploy this image to every daemon before PR 6’s manager lists providers without legacy channels.
After PR 6 deploys, this image is the daemon rollback floor.
Roll the manager back below PR 6 before deploying an older daemon image.
The existing GitHub credential boundary follows PR #204’s final outcome.
Workspace sources never add credentials or widen GitHub token permissions.

The image contract test registers two non-Git roots against the real daemon.
It checks that source context produces no checkout, both routing roots stay empty, and upstream displays the slug literally.
Run `bash voltage/images/paseo-daemon/test-image.sh <image>` in a Docker-capable environment.
