# Paseo workspace records

Use `buzz paseo workspace` with the owner's direct signing key to manage channel-bound workspace records.
A workspace can have zero repositories.
These commands do not create a channel, repository, or daemon directory.
The bridge and daemon integrations consume these records in subsequent rollout steps.

```sh
buzz paseo workspace create --name "Payments Review" --channel <channel-uuid>
buzz paseo workspace create --slug review --channel <channel-uuid> \
  --source https://github.com/voltagecloud/buzz
buzz paseo workspace list --owner <owner-hex>
buzz paseo workspace update --slug review --name Review --clear-sources
buzz paseo workspace delete --slug review
```

Supply `--name`, `--slug`, or both. An explicit slug always wins and must match
`^[a-z0-9][a-z0-9._-]{0,63}$`; invalid values fail before relay access.
Without `--slug`, creation lowercases ASCII letters and replaces runs outside `[a-z0-9]` with one hyphen.
It trims separators, truncates to 64 characters, and trims trailing separators again.
An empty result requires an explicit slug; there is no transliteration or hash fallback for creation.
For example, `Payments Review` produces `payments-review`, while the `name` tag keeps `Payments Review` verbatim.
The response includes `slug` and states that it is the name shown in the Paseo project list.
If a derived slug already exists for this owner, creation reports the conflict; it never adds a suffix.

`create` writes an owner-signed NIP-MP project (`kind:30621`) with these Voltage extension tags:

| Tag | Meaning |
| --- | --- |
| `buzz-paseo = workspace` | Distinguishes workspaces from ordinary forge projects. Exactly one marker is required. |
| `buzz-channel` | One unarchived stream or forum channel. |
| `buzz-source` | A GitHub HTTPS repository URL; repeatable, up to 16 entries of 256 bytes each. |
| `buzz-visibility` | Defaults to `unlisted`; `--listed` makes the project discoverable in the collection. |

**Workspace metadata is community-readable, including unlisted records.**
Names, channel UUIDs, and source repository names are visible to community readers.
The CLI prints this warning before creation.
Sources are prompt context, not local clones or links.
Sources grant no GitHub access and do not change the token's permissions.

Optional `--repo` arguments add existing NIP-34 member coordinates to the project card.
Without `--repo`, the workspace has no members and creates no repository.
`update` preserves member coordinates, descriptions, content, and unknown tags.
An unmarked project cannot be updated, deleted, or overwritten through workspace commands.
NIP-OA agent credentials cannot write workspaces; authenticated readers can use `list --owner`.

Creation checks the channel, then adds each live delegated gateway only when its membership is missing.
The owner must already belong to the channel.
A failed membership step returns an error before the workspace is published.
Rerun the same command to finish; existing memberships remain in place.
An identical create with an explicit `--slug` returns `unchanged: true` and checks membership again.
A name-derived create always rejects an existing slug, including identical metadata.
Changing a workspace's channel also prepares gateway membership in the new channel.
Deletion never removes channel memberships.

The CLI checks agents in the owner's signed agent listings and any repeated `--agent` arguments.
It warns when an agent is missing from the channel or belongs to more than 64 channels.
The latter can exceed the mention launcher's watch limit.
Warnings appear on stderr and in write-response JSON.

## Shared reader contract

`crates/buzz-cli/src/commands/paseo/workspaces.rs` owns parsing, strong relay reads, selection, and directory naming.
The CLI uses `load(client, owner)`; the daemon consumes `workspace list --owner`.
The bridge can use `select`, `metadata_ids`, and `READ_BOUND` with its own relay transport.
Both receive the same validated selection.
Read failures return an error; callers must preserve their previous state.

List stdout is a JSON array with these fields:

| Field | Type |
| --- | --- |
| `owner` | Hex public key |
| `slug`, `name`, `directory` | Strings |
| `channel` | Canonical UUID string |
| `sources` | Array of GitHub URL strings |
| `created_at` | Unix seconds |

The reader verifies each workspace's owner and signature and validates its project envelope and channel.
It orders valid heads by newest timestamp, then lowest slug.
It accepts at most 32 workspaces and 64 distinct sources, skipping any workspace that exceeds either cap.
Skipped records are reported on stderr by slug.
Visibility does not affect selection.

A slug matching `^[a-z0-9][a-z0-9._-]{0,63}$` is its directory name.
Other slugs use `_` followed by the first 16 hex digits of SHA-256 of the slug's UTF-8 bytes.
Literal names cannot start with `_`.
If two workspaces map to one directory, both are skipped before caps are applied.
Directory names are relative names beneath the daemon's workspace root.
