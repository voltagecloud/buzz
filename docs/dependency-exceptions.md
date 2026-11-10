# Temporary SQLite dependency exception

The accepted exception for `RUSTSEC-2026-0459` affects shipped desktop builds and the workspace Mesh smoke harness.
It is not limited to development dependencies.
Review it by **2026-11-24** in [tracking issue #177](https://github.com/voltagecloud/buzz/issues/177).

## Reachability

The pinned Mesh-LLM stack uses `rusqlite 0.40.2` and bundled `libsqlite3-sys 0.38.2`.
The desktop release workflow enables `mesh-llm` on macOS and Linux.
Desktop also depends on `rusqlite` directly, including builds without the Mesh feature.

The advisory requires opening a crafted database and sibling write-ahead log (WAL) with mismatched page sizes.
Ordinary queries can then expose uninitialized process memory.
Mesh opens its local `log_store.db` with WAL enabled.
Buzz desktop also opens local archive, unread, and channel-head databases.

These are application storage paths, but we have not proved every attacker-controlled file path unreachable.
The exception accepts this residual risk while the upstream dependency remains incompatible with the patched stack.
Keep database and WAL files in trusted local storage.
Reassess any new import or restore path before it can open supplied database files.

## Removal

1. Check whether a Mesh-LLM release supports `rusqlite 0.41` and `libsqlite3-sys 0.39` or newer patched versions.
2. Update the manifests that pin the vulnerable stack: the Mesh-LLM tags in `desktop/src-tauri/Cargo.toml` and `crates/buzz-mesh-smoke/Cargo.toml`, the direct `rusqlite = "0.40"` constraint in `desktop/src-tauri/Cargo.toml`, and the `sqlx-sqlite` revision in the root `Cargo.toml` `[patch.crates-io]`, which must accept the patched `libsqlite3-sys`.
3. Regenerate the workspace and desktop lockfiles together.
4. Remove this advisory from `deny.toml`, `deny-mesh.toml`, and `deny-desktop.toml`.
5. Run the workspace, Mesh, and desktop dependency policies.
6. Close the tracking issue with the patched versions and validation results.

The exception is temporary; its review date is not an automatic claim that the vulnerability becomes unreachable.
See the [RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0459.html).

Evidence inspected: `.github/workflows/release.yml`, `desktop/src-tauri/Cargo.toml`, and the pinned Mesh `mesh-llm-log-store/src/store.rs`.
Desktop consumers include `archive/store.rs`, `observed_unread.rs`, and `channel_head_cache.rs` under `desktop/src-tauri/src`.
