# Vendored apps

## `buzz-gpui`

`apps/buzz-gpui` is a snapshot of
[benthecarman/buzz-gpui](https://github.com/benthecarman/buzz-gpui), a native
GPUI client for Buzz communities (Apache-2.0; see its `LICENSE` and
`NOTICE.md`). Voltage ships it as the Buzz client. The Tauri app in `desktop/`
stays only so that upstream `block/buzz` syncs merge cleanly; it is not built
or shipped.

`apps/buzz-gpui` is its own Cargo workspace. The root workspace does not
include it, and changes under `apps/` do not select any `block/buzz` CI suite.

`UPSTREAM` records the upstream repository, ref, and commit of the snapshot.
The import keeps no upstream history: each sync is one signed, signed-off
commit, as `main` requires.

### Sync from upstream

```sh
scripts/sync-buzz-gpui.sh v0.3.0   # a tag, branch, or commit; default: master
```

The script applies only the upstream diff between the recorded commit and the
new ref, as a three-way merge, so Voltage changes under `apps/buzz-gpui`
survive. It stages the result and updates `UPSTREAM`. Review the diff, then
commit with `git commit -S -s`.

If the script reports conflicts, resolve them, `git add` the files, run
`scripts/sync-buzz-gpui.sh --record <ref>`, and commit.

Keep Voltage changes to upstream files small and grouped, so syncs stay easy
and changes can be offered back upstream.
