# Brief for contributors working in a worktree

Read first: ARCHITECTURE.md, docs/port/ui.md (visual spec), docs/port/relay.md
(data spec). The reference app is at /home/ben/projects/buzz-app (read-only).

## Build and test
- Always build through Nix: `nix develop --command cargo build|test|clippy ...`.
- Clippy must report no warnings for the crates you touch:
  `nix develop --command cargo clippy -p <crate> --all-targets`.
- Run `nix develop --command cargo fmt --all` before committing. A pre-commit
  hook enforces it.
- Relay integration tests need `BUZZ_TEST_RELAY=/home/ben/projects/buzz-gpui/.test-relay`.
  The plain relay runs at `ws://127.0.0.1:53000`. A TLS relay runs at
  `wss://localhost:53443` (see `scripts/TEST-RELAY.md`). Never start, stop or
  wipe the relays. Identities (alice/bob/carol) are in `.test-relay/identities.json`;
  channel ids are in `.test-relay/seed.json`.

## Seeing the UI (the machine is headless)
Create your own scratch directory, e.g. `T=/tmp/<your-task>`. Then:
1. `mkdir -p $T/cfg/buzz-gpui $T/data/buzz-gpui` and write
   `$T/cfg/buzz-gpui/config.toml` with `[embedded]\ncredentials = "files"\nworkspace = "$T/ws"`.
2. Put alice's nsec in `$T/alice.nsec`.
3. Run the app once so it creates `$T/data/buzz-gpui/buzz.sqlite3`. Then seed the
   community and the selected channel with sqlite3 (`nix build --no-link
   --print-out-paths nixpkgs#sqlite.bin`):
   `INSERT OR REPLACE INTO viewer_prefs VALUES ('<alice pk>','communities','{"origins":["ws://127.0.0.1:53000"],"selected":"ws://127.0.0.1:53000"}');`
   `INSERT OR REPLACE INTO view_state VALUES ('http://127.0.0.1:53000','<alice pk>','selected-channel','"<channel id>"',0);`
4. Take a screenshot with Xvfb and Mesa lavapipe:
   ```
   MESA=$(nix build --no-link --print-out-paths nixpkgs#mesa); XVFB=$(nix build --no-link --print-out-paths nixpkgs#xvfb-run)
   XWD=$(nix build --no-link --print-out-paths nixpkgs#xorg.xwd); MAGICK=$(nix build --no-link --print-out-paths nixpkgs#imagemagick)
   nix develop --command bash -c "VK_ICD_FILENAMES=$MESA/share/vulkan/icd.d/lvp_icd.x86_64.json WAYLAND_DISPLAY= \
     XDG_CONFIG_HOME=$T/cfg XDG_DATA_HOME=$T/data BUZZ_DEV_NSEC_FILE=$T/alice.nsec \
     $XVFB/bin/xvfb-run -a -s '-screen 0 1400x900x24' bash -c './target/debug/buzz-gpui & P=\$!; sleep 8; $XWD/bin/xwd -root -silent | $MAGICK/bin/magick xwd:- $T/shot.png; kill \$P'"
   ```
   Look at the PNG with your Read tool, and compare it with the reference
   screenshots in /home/ben/projects/buzz-gpui/docs/port/screenshots/.
   Mouse clicks can be scripted with `xdotool` (nixpkgs#xdotool) inside the
   same xvfb-run shell.
5. Development only: set `BUZZ_DEV_OPEN_THREAD=<event id>` to open that
   thread panel beside the first channel the app selects, e.g. the
   `events.welcome` root from `seed.json` in #general.

## Commits
- Commit on your worktree branch in small, logical commits.
- Subject: at most 50 characters, capitalized, imperative mood, no trailing period.
- Leave a blank line, then a body wrapped at 72 that explains what and why.
- No links to GitHub issues or commits.
- End with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Match the surrounding code: doc comments explain *why*, and stay concise.

## Report back
Your branch, your commits, what you built, the screenshots you took (paths), and
anything left undone or deviating from the spec.
