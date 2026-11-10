# Snapshot archive validation

The snapshot archive reader validates authenticated plaintext before a consumer
writes files or imports Git objects. The producer format and scan policy are
specified in [sandbox-snapshot-producer.md](sandbox-snapshot-producer.md).
The transport uses [NIP-SA](nips/NIP-SA.md).

`buzz_cli::sandbox_archive::inspect_archive` returns an opaque `ArchivePlan`.
It performs no filesystem writes, Git execution or network access. A successful
plan proves the archive structure fits the fixed limits below. A restore
consumer must also verify Git integrity, account for local storage, and publish
the mirror atomically. This foundation does not expose a user restore command.

| Resource | Limit |
| --- | --- |
| Compressed archive | 100 MiB |
| Declared regular payloads | 256 MiB total |
| Expanded Git objects | 256 MiB total, including delta targets |
| Git objects | 200,000 total |
| Repositories | 64 |
| Manifest | 8 MiB |
| Copied, omitted and deleted paths | 50,000 total |
| Advertised Git refs | 50,000 total |
| Tar entries | 150,000 |
| Local PAX payload | 16 KiB per header, 1 MiB total padded metadata |
| Relative path | 4,096 UTF-8 bytes and 128 components |
| Symlink resolution | 128 substitutions |
| Parsing | 120 seconds, with cancellation checks |

The reader iterates raw tar headers. It checks extension type and declared size
before reading PAX payloads, so the tar library cannot allocate an unbounded
extension body. Only local `path` and `linkpath` PAX fields are accepted. GNU
extensions, sparse files, hard links, devices, FIFOs, global PAX fields and
unknown fields are refused. Gzip CRC, end markers and trailing bytes are checked;
concatenated gzip members are refused.

Paths must be canonical relative UTF-8 paths. Duplicate tar entries, missing or
undeclared files, file ancestors and repository paths crossing files are
refused. Working files follow the checkpoint exclusion policy. Versioned
excluded paths can remain in the scanned bundle history. The validator does
not check out that history. Case variants of `.git` are also refused as copied
paths, to protect repository metadata on case-insensitive filesystems.

The reader preserves full locally available history and explicit shallow
boundaries. It requires a scanned snapshot manifest and uses the SDK pack
preflight for each bundle before accounting for aggregate object counts and
expanded sizes. The restore consumer must still run Git integrity checks with
replacement refs, hooks, lazy fetch and external configuration disabled.

Relative symlinks must stay within their repository. The validator resolves
symlink ancestors to reject cycles and excessive chains. A nested repository
owns a copied file when both its own bundle and an outer repository name the
same physical path. Consumers must defer creating links until all regular files
and Git metadata have been written.

## Read-only restore

`buzz_cli::sandbox_archive::check_git().await` checks platform support and Git
2.29 or newer before a caller publishes a snapshot request. Linux and macOS are
supported. Paseo requires `host-tools=on` for restore and explorer Git commands.
The probe preserves the Git-denying PATH wrapper when host tools are off.

`extract` owns the authenticated archive bytes and accepts a fresh destination,
store root, `RestoreBudget`, and cancellation token. The destination parent must
already exist inside the store. Existing destinations, including empty
directories, are refused. The output lists repository roots for explorer
history and diffs.

```mermaid
flowchart LR
  A[Validate archive] --> B[Reserve storage]
  B --> C[Stage files and bundles]
  C --> D[Import and verify Git]
  D --> E[Create safe links]
  E --> F[Freeze permissions]
  F --> G[Publish fresh directory]
```

Restore serializes operations within a store. It charges existing store usage,
new file and directory blocks, temporary bundles, imported packs, indexes and
an operational reserve. Both explicit work/store budgets and actual filesystem
free space must permit the operation. Filesystem free space does not reveal a
Kubernetes emptyDir quota. Configure budgets below that quota. The defaults
allow 128 MiB for the operation and the total store; explicit limits cannot
exceed 2 GiB each.

Git imports run with fixed arguments and no network, hooks, templates,
replacement refs, lazy fetch or external configuration. Pack preflight happens
before import. Full Git integrity checks and actual object inventories must
match the preflight before index creation. Ref creation uses [Git's NUL-delimited transaction format](https://git-scm.com/docs/git-update-ref) and rejects null object IDs. Restore reads HEAD into the index
without checking out files. It marks excluded tracked paths skip-worktree, so
omitted private files do not appear as deletions. Genuine working-tree
deletions remain visible in diffs. Local branches, tags and declared shallow
boundaries are preserved without fetching unavailable ancestors.

Each Git operation has bounded output and a 60-second deadline. Restore has a
180-second deadline. Process-group teardown retains the leader until after the
stop signal, preventing process-ID reuse during cleanup. Scratch directories
are private and beside the destination. Safe symlinks are created after files
and Git metadata. Files and directories become owner-only and read-only before atomic
publication with no replacement of an existing destination.

Cancel by cancelling the token, then await `extract` to completion. Do not abort
or drop its future. The function stops and reaps Git children and removes
staging before returning cancellation. Failures leave the destination absent;
a concurrent creator keeps its own destination. Cleanup failures propagate.
Once publication succeeds, the local copy remains available after remote grant
expiry. Expiry prevents new transfer and receipt use; it cannot erase bytes
already delivered to an authorized reader.
