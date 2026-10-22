# Remote developer agent image

Run the Buzz harness and developer tools inside a Kubernetes [Agent Sandbox](https://agent-sandbox.sigs.k8s.io/docs/sandbox/custom_sandbox/image/).
Build Linux `arm64` and `amd64` images for the five initial sessions owned by `tee8z`.
Staging agent workers are Graviton (`arm64`); the tool downloads in the Dockerfile are pinned per architecture.
Each developer supplies their own accounts at runtime.

The image contains Codex and Claude ACP adapters, Rust, Node, Go, Python, Git, GitHub CLI, `via`, and SQL clients.
Buzz selects the runtime, model, persona, and arguments for each session.
The image builds the Buzz source of the commit it is built from; npm dependencies use the committed lockfile.

| Repository | Responsibility |
| --- | --- |
| [Terraform](https://github.com/voltagecloud/voltage-terraform/pull/952) | Worker capacity, developer namespaces, quotas, and ECR |
| [CD](https://github.com/voltagecloud/cd/pull/628) | Upstream Sandbox controller and CRD; Alloy collection and Grafana dashboards |
| [Buzz](https://github.com/voltagecloud/buzz/pull/1) | Owner and thread binding, Sandbox creation, runtime control, and JSONL action events |
| This directory (`voltage/images/buzz-agent` in voltagecloud/buzz) | Developer environment, account helpers, and container entrypoint |

The SDK example also uses a router and warm pool. Buzz uses the Kubernetes Sandbox API directly, so this image runs the harness as its main process.
Credentials stay out of the image. Runtime binaries remain outside the disposable `/home/agent` workspace.
The process runs as `10001:10001` and receives shutdown signals through `tini`.

## Build and test

The build context is the repository root. `Dockerfile.dockerignore` next to the Dockerfile selects it.
From the repository root:

```bash
docker build --platform linux/amd64 -f voltage/images/buzz-agent/Dockerfile -t buzz-agent:local .
cd voltage/images/buzz-agent
bash test-image.sh buzz-agent:local
bash test-harness.sh buzz-agent:local
python3 -O test-native-agent.py buzz-agent:local
python3 test-policy.py buzz-agent:local
python3 test-accounts.py buzz-agent:local
python3 test-checkpoint.py buzz-agent:local
python3 test-checkpoint-storage.py buzz-agent:local
python3 test-restore.py buzz-agent:local
python3 test-gate.py buzz-agent:local
python3 test-worker.py buzz-agent:local
python3 test-cache.py buzz-agent:local
python3 -O test-profile.py buzz-agent:local
```

These checks use synthetic tasks and accounts. Live GitHub, signing, `via`, cache, and staging behavior require end-to-end validation.
Run the bundled Rust suites with `docker build --platform linux/amd64 --target package-checks -f voltage/images/buzz-agent/Dockerfile .`.
Build and test `linux/arm64` on an arm64 host, or through `.github/workflows/voltage-agent-images.yml`, which builds each platform on a native runner.

## Sandbox contract

Use an immutable registry digest in `spec.podTemplate.spec.containers[].image`.
Mount a disk-backed workspace at `/home/agent`; configure resource bounds and a shutdown grace period in the Pod template.
Set `BUZZ_ACP_ACTIVITY_LOG=true` to emit tool and lifecycle records for external Alloy collection.
Alloy retains sanitized events in Loki; workspace deletion must not delete the collected history.

The provider supplies `BUZZ_SANDBOX_POD_UID` through the downward API and binds its Secret to `BUZZ_SANDBOX_INITIAL_POD_UID`.
If these UIDs differ, the entrypoint refuses startup. A replacement Pod requires explicit workspace recovery.
New sessions load the developer's current account profile.
Checkpoints use the developer's S3 prefix and service account from Terraform.
The harness stops adapter processes before invoking `buzz-agent-checkpoint save`, including cancellation during startup.
A failed save retains the Sandbox for recovery; do not delete its workspace.

Explicit checkpoint recovery sets `BUZZ_CHECKPOINT_RESTORE_KEY` in the launch Secret; it is empty otherwise.
After the developer profile bootstrap and before the harness starts, the entrypoint runs `buzz-agent-checkpoint restore-once`.
The key must be under the developer's `BUZZ_CHECKPOINT_PREFIX`. A failed restore stops startup; the harness never starts with an empty workspace.
The restore moves repositories into `/home/agent` without overwriting existing paths, then writes `.config/buzz-agent/checkpoint-restored.json`.
A container restart with that marker skips the restore and keeps newer edits.
After a successful save, the harness writes `{"version":1,"checkpoint":"<key>"}` to `/dev/termination-log` for the reaper.

Sub-agents run as short-lived worker Sandboxes, each in its own Pod, so one busy or failing worker does not stop the others.
An orchestrating session with the Buzz `subagent_token` Pod option gets `buzz-subagent` credentials at `/var/run/secrets/buzz-subagents`; `buzz-agent-profile run` then appends `/etc/buzz-agent/subagents.md` to the Claude Code and Codex global instructions.
`buzz-subagent spawn` creates a SandboxClaim against the developer's `agent-worker` pool with `BUZZ_WORKER_*` variables; `list`, `status`, `wait`, `result`, and `cancel` follow it.
A worker runs `buzz-agent-profile work`: the same profile bootstrap, then `buzz-agent-worker`, which clones the repository, works on `worker/<task>` from the base branch with Claude Code or Codex non-interactively, commits any remaining edits, pushes the branch, writes `<developer>/workers/<task>/result.json`, and exits with the runtime's status.
Worker activity records carry metadata only, not the focus or runtime output.
Worker lifetimes must be at least three minutes, including two minutes reserved for publishing results.
The runner retains only the final 4,000 output characters and stops the runtime process group before inspecting the checkout.
Timeouts return 124; SIGTERM returns 143 after stopping the runtime's children.
Result reads distinguish missing results from AWS permission or connection failures.

Heavy jobs share each worker through a node-local gate. `/opt/buzz-heavy/bin` is first on `PATH`; its `cargo` and `go` wrappers run compiling commands (`build`, `check`, `clippy`, `test`, `run`, and similar) through `buzz-heavy`, which holds one of the node's slots while the command runs.
Other heavy commands, such as browser test suites, can use `buzz-heavy -- <command>` directly. Nested calls reuse the outer slot.
The gate is the same image run as `buzz-agent-gate` by a DaemonSet on agent workers (`BUZZ_HEAVY_SLOTS`, default 2), reached at `BUZZ_HEAVY_GATE` (default `agent-gate.buzz-agent-gate.svc.cluster.local:7070`).
A slot is released when its connection closes, including when the job crashes. If the gate is unreachable, the command runs without admission and prints a warning.

Codex uses `BUZZ_ACP_PERMISSION_MODE=agent-full-access` inside the restricted Kubernetes container.
The harness requires the adapter to advertise and confirm this mode before sending a prompt.
Developer tools and configured MCP servers remain available; the previous text-only restriction is removed.

## Developer accounts

Each developer enrolls one profile from the Buzz app (GPUI), through the Sandbox manager over the relay. Every one of their Sandboxes and workers loads it.
Terraform creates the empty Secrets Manager secret per developer; the manager writes it, and the developer's `agent-workspace` and `agent-worker` service accounts can only read its exact ARN.
The manager sets `BUZZ_DEVELOPER_ID` and `BUZZ_DEVELOPER_PROFILE_ARN` in each launch from its developer config, and fills in the profile's `developer` from the signing owner's entry.
[profile.example.json](profile.example.json) shows the stored shape.

The bootstrap checks the profile against Buzz's verified owner, then writes Git identity, GitHub, via, the agent signing key, and the vault's Depot credential to private workspace files.
Missing profiles, owner mismatches, and unsafe paths refuse startup. A new enrollment takes effect for new sessions; running sessions keep their credentials, so revoke upstream tokens when required.
Credentials are excluded from checkpoints. The image test uses a Secrets Manager emulator.

`models` accepts these optional credentials:

| Field | Account | Runtime |
| --- | --- | --- |
| `openai_api_key` | OpenAI API billing | `OPENAI_API_KEY`; Codex login through its CLI |
| `codex_access_token` | ChatGPT Business or Enterprise seat | `CODEX_ACCESS_TOKEN` |
| `anthropic_api_key` | Anthropic API billing | `ANTHROPIC_API_KEY` |
| `claude_oauth_token` | Claude subscription seat | `CLAUDE_CODE_OAUTH_TOKEN` |
| `openrouter_api_key` | OpenRouter billing | `OPENROUTER_API_KEY`; `BUZZ_AGENT_PROVIDER=openrouter` unless the agent sets one |

To use both subscriptions in parallel, enroll `codex_access_token` and `claude_oauth_token` together. Neither has a refresh token, so any number of Sandboxes can use them at once.
- **Codex:** a workspace owner allows [Codex access tokens](https://learn.chatgpt.com/docs/enterprise/access-tokens); the developer creates one (`at-…`) at `chatgpt.com/admin/access-tokens`, within the workspace's expiry limit. Re-enroll before it expires.
- **Claude:** run `claude setup-token` on the developer workstation and store the one-year token.

A profile can contain `openai_api_key` or `codex_access_token`, not both: Codex reads the access token first, so mixing them would hide which account runs.
A profile can contain `anthropic_api_key` or `claude_oauth_token`, not both. Claude Code prefers an API key, so mixing them would bill the API silently.
`openrouter_api_key` serves the Buzz agent runtime (`buzz-agent`), which can run any OpenRouter model: set the agent's model to an OpenRouter model ID such as `qwen/qwen3-coder`.
The bootstrap refuses an inherited `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, or `OPENROUTER_API_KEY`.
Model credentials stay in the harness environment and are never written to workspace files. Tools that the agent runs inherit them.
The checkpoint secret scan also refuses Claude subscription tokens.

### Commit signing

Every commit and tag is signed with an agent-only SSH key, `git.ssh_signing_key` in the profile: one OpenSSH Ed25519 private key.
The GPUI enroll form generates it with `ssh-keygen`. Once the manager stores the profile, the developer's local GitHub CLI (`gh`) adds the public half to their GitHub account as a signing key; otherwise the form shows the key and the steps to add it. Commits then show as Verified with the developer's verified Git email. The agents' GitHub token needs no account permission.
The bootstrap writes it to `.config/buzz-agent/ssh-signing-key` (mode `0600`, excluded from checkpoints), and `buzz-agent-account init` configures `gpg.format ssh` with `commit.gpgsign` and `tag.gpgsign`. Without the key, Git refuses to commit.
The developer's own GPG key never leaves their device. Revoke agent signing by deleting that key under GitHub → Settings → SSH and GPG keys; enrolling again creates a new one.

### Depot build cache

Every developer's agents share one Depot organization token, kept in the development vault.
The via profile names it with a `depot` service; at startup the bootstrap reads both values with the profile's 1Password service account token:

```toml
[services.depot]
description = "Depot build cache"
provider = "onepassword"
secrets.organization = "op://Buzz Agents Dev/Depot/organization"
secrets.token = "op://Buzz Agents Dev/Depot/credential"
```

Without the service, or when the vault read fails, the session starts without the remote cache and logs `Depot cache unavailable`.
The harness and its tools inherit the cache configuration.
The image pins Depot CLI and sccache; each repository selects its Turborepo version.
Rust uses sccache's WebDAV backend, Turborepo uses `TURBO_API`, and Go 1.24+ uses `depot gocache`.
Credentials stay in private runtime files and are excluded from workspace checkpoints.
[Depot cache authentication](https://depot.dev/docs/cache/authentication) requires a user or organization token; project tokens do not work.

For acceptance, build the same revision twice from separate empty build directories or fresh Sandboxes.
Record elapsed time and remote hits/misses for Rust, Turborepo, and Go. Match compiler versions, build flags, and paths.
Verify that a revoked token cannot read or write the cache. A successful local build alone does not prove remote reuse.

To test the Rust cache with an existing private Depot environment file:

```bash
python3 voltage/images/buzz-agent/test-depot.py buzz-agent:local \
  --depot-env "$HOME/.config/depot/sccache.env"
```

This builds a unique fixture in two fresh containers. The first must upload; the second must report a remote hit without cache errors.
The test prints only counts and elapsed time. It does not persist credentials outside the disposable containers.

## Publish to staging

Each merge to `main` that changes the image inputs publishes this image to `voltage-buzz-agent-staging`, tagged with the full commit SHA.
See [../README.md](../README.md) for the workflow, the digest to pin, and where to pin it.
The voltage-terraform admission policy admits only digest-pinned images from `voltage-buzz-agent-staging` in developer namespaces.
