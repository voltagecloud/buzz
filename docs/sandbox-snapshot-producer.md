# Snapshot archive producer

`buzz-agent-snapshot` builds a fully scanned archive inside an agent sandbox.
This layer packages a local artifact only. The ACP request handler, encrypted Blossom transfer, Rust extraction, and user-facing CLI follow separately.
The caller must hold a live owner-approved snapshot grant and run the helper under the isolated worker's containment and cancellation rules.
No terminal approval or global gateway delegation alone permits export.

```sh
snapshot_job="$(mktemp -d /tmp/buzz-snapshot-job.XXXXXX)"
buzz-agent-snapshot --workspace /workspace --output "$snapshot_job/archive.tar.gz" --work-bytes 536870912
```

The output must be new. Its existing parent must be a private caller-owned job directory.
Scratch is created explicitly inside that parent, independently of `TMPDIR` or Python's cached temporary directory.
If the parent is inside the captured workspace, it must match checkpoint exclusions, such as `.local/state`.
That private subtree is excluded before bounded Git file enumeration. Visible in-source output parents are refused.
The output is mode 0600 and is removed if creation fails.
After forced cancellation, the caller removes the entire job directory through its durable cleanup record.
Stdout contains only version, scan status, repository count, and byte count.
Failures report only `scan`, `budget`, or `archive`; scanner output and file names are suppressed.
The helper never uploads, fetches from a forge, signs relay events, or falls back to quarantine.

Every repository gets a self-contained `git bundle --all HEAD` plus its tracked and non-ignored untracked working files.
Nested repositories have separate records and bundles; Git's directory markers are not copied as files.
The manifest records `version: 1`, `snapshot: true`, `scanned: true`, and repository path, head, branch, explicit shallow boundaries, file list, exclusions, and deletions.
Every record uses `base: bundle`; there is no origin URL or clone prerequisite.
A workspace that is itself a repository uses the output path `workspace`.

The tar layout reuses the checkpoint structure:

```text
snapshot/manifest.json
snapshot/repositories/0/history.bundle
snapshot/repositories/0/files/<relative paths>
```

The user chose full locally available scanned history.
Historical versioned content, including a historical `.env.example`, remains in the bundle.
The working copy excludes checkpoint private/runtime paths and `.env*` files.
The next extractor must mark intentionally omitted tracked paths skip-worktree so they do not appear as accidental deletions; genuine source deletions remain absent.
Shallow clones retain their explicit local boundaries, without unshallow or network fetch.
Unborn HEADs are refused.

Before packing, the helper inventories source objects and byte/count budgets.
After capture, the required local `buzz sandbox inspect-bundle` helper validates the actual bundle with the SDK before any Git import, independently of credentials or relay configuration.
The producer reserves the captured expansion, object/ref/index/block costs and aggregate counts, then verifies the neutral inventory matches before writing raw review bodies.
A source commit between preflight and capture therefore cannot inherit stale scratch admission; missing or malformed inspector output fails closed.
It imports the bundle into a neutral Git repository, verifies its object graph, and scans that exact history with immutable gitleaks rules.
It also scans the copied working files, manifest, ref names, and every archived raw object, including merge-only blobs and annotated-tag messages.
Global/system configuration, replacement refs, grafts, lazy fetch, hooks, fsmonitor, repository ignore files, and `GITLEAKS_*` overrides cannot weaken the scan.
Repository and file reads use open directory descriptors and refuse source path redirection through symlinks.
Internal relative symlinks are preserved; external links and special files are refused.

Limits are 256 MiB source/expanded object bytes, 200,000 objects, 50,000 files/references, 64 repositories, and 180 seconds total creation time.
Subprocess output and time are bounded, and Git object bodies use one streamed batch.
Bundle and gzip output are admitted before writes; the final archive leaves space for the encrypted blob prefix/tag within 100 MiB.
The local work budget defaults to 512 MiB and may be set from 1 MiB through 2 GiB; filesystem free space can lower it.
The budget counts object review files, bundles/imports, copied files, block/index overhead, compressed output, and an operational reserve.
`statvfs` cannot reveal Kubernetes emptyDir sizeLimit: configure the work budget below the actual scratch quota after accounting for other agent data.
The separate attaching-machine extractor also needs its own store/work budget and capacity checks.

Tar ownership and timestamps are normalized. UTF-8 path/link metadata is capped at 16 KiB per header and 1 MiB total; the next extractor must apply bounds before extension allocation.
The archive is validated and scanned after copying; source work can continue during capture, so it is a bounded working view rather than a filesystem transaction.

Run `python3 voltage/images/buzz-agent/test-snapshot.py <agent-image>` to exercise packaged code and the actual scanner with no network, a read-only root, UID 10001, dropped capabilities, and a 1 GiB memory cap.
The tests cover SHA-1/SHA-256, local branches, nested repositories, dirty/deleted files, executable modes, relative links, long PAX names, shallow boundaries, source/capacity refusal, and current/deleted/merge/tag secret refusal with ignore bypass attempts.
The shared policy fixture is checked against checkpoint constants and will also bind the Rust extractor.

Run `python3 voltage/images/buzz-agent/test-snapshot-scratch.py <agent-image>` for the real SIGKILL and scratch-path regression.
It covers a cached foreign temporary root, caller cleanup, visible-parent refusal, and private-subtree file-list limits.
