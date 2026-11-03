# Buzz GPUI

A native client for [Buzz](https://github.com/block/buzz) communities, written
with [GPUI](https://github.com/zed-industries/zed). It covers the chat and
agent-management parts of [buzz-app](https://github.com/block/buzz-app) with the
same look. It can also manage agents running on other machines through
`buzzd`, a small agent host daemon.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

Voltage builds also support moving saved teams or selected agents through **Agents → Your agents → Move to remote**.
See [Move existing agents to Remote workspace](docs/remote-agent-migration.md) for setup, preserved settings, and recovery.

## Laptop: install the app (Ubuntu)

```sh
sudo apt install ./buzz-gpui_0.1.0-1_amd64.deb
```

Build the package with `scripts/package-deb.sh`. It builds inside an
`ubuntu:26.04` container and writes `dist/buzz-gpui_<version>_amd64.deb`.

On first start the app reads your identity from the keyring. It uses the
`dev.local.buzz.foundation.identity` entry. If that entry is empty, it copies
the identity once from buzz-app's combined entry (`buzz-desktop`), and leaves
buzz-app's entry as it was. macOS can ask you to allow access to that entry.
If you have never used buzz-app on this machine, import your `nsec` or create
a new identity. Then add your
community by its relay origin, for example `wss://relay.example.com`.

Settings → Appearance → System tray puts a Buzz icon in the panel and can
keep Buzz running there when its window is minimized or closed. GNOME shows
tray icons through the AppIndicator extension, which Ubuntu turns on.

Agents on the laptop run inside the app. Their keys are kept in the system
keyring, and they stop when the app quits.

## Laptop: install the app (macOS)

The macOS build is not signed or notarized. It carries the GStreamer
libraries and plugins it plays audio and video with, so nothing else has
to be installed.

1. Open `Buzz-<version>-macos-arm64.dmg` (Apple silicon) or
   `Buzz-<version>-macos-x86_64.dmg` (Intel).
2. Drag Buzz to Applications.
3. The first time, right-click Buzz in Applications and choose Open. If
   macOS still refuses, run `xattr -dr com.apple.quarantine /Applications/Buzz.app`.

Build the image on a Mac with `scripts/package-macos.sh`. It writes
`dist/Buzz-<version>-macos-<arch>.dmg` for the Mac's own architecture. It
needs the official GStreamer framework: install the runtime and
development packages (`gstreamer-1.0-<version>-universal.pkg` and
`gstreamer-1.0-devel-<version>-universal.pkg`) from
[gstreamer.freedesktop.org](https://gstreamer.freedesktop.org/download/).
The script copies the libraries and plugins the app uses into `Buzz.app`.
`scripts/check-macos-dmg.sh <dmg>` then checks that the app loads nothing
from outside itself and the system, and plays media with the framework
moved aside (it uses `sudo`).

## Laptop: install the app (Windows)

The Windows build is not signed. It carries the GStreamer libraries and
plugins it needs, so nothing else has to be installed.

1. Extract `Buzz-<version>-windows-x86_64.zip`.
2. Run `Buzz\bin\buzz-gpui.exe`. If SmartScreen stops it, choose More info,
   then Run anyway.

Keep the `bin` and `lib` folders together: GStreamer finds its plugins in
`lib` next to `bin`. The zip also holds `buzzd.exe`.

Build it on Windows with GStreamer's MSVC runtime and development
installers, `cargo build --release -p buzz-gpui -p buzzd`, then
`python scripts/package-windows.py <GStreamer root>` (needs `pip install
pefile`). It writes `dist/Buzz-<version>-windows-x86_64.zip`.

### What differs on macOS and Windows

[docs/port/PLATFORMS.md](docs/port/PLATFORMS.md) lists every difference.
The ones you may notice:

- macOS shows notifications only for Buzz run from `Buzz.app`, and asks
  for permission on the first one.
- Windows has no channel terminals, and Buzz cannot install Pi or Claude
  Code there; install them yourself and Buzz finds them on `PATH`.
- On Windows, hosts and `buzzd` listen and connect over `tcp://` only, not
  `unix://`.
- HEIC avatars need `ffmpeg`: `brew install ffmpeg` on macOS, and on
  Windows an `ffmpeg.exe` on `PATH`.

### Builds

The Build workflow makes all of these packages. It runs by hand, for pull
requests that change it, and for `v*` tags, which also attach the packages
to a GitHub release.

## Dev server: run `buzzd` (NixOS)

Add the flake as an input and enable the module:

```nix
{
  inputs.buzz-gpui.url = "git+file:///home/ben/projects/buzz-gpui";

  outputs = { nixpkgs, buzz-gpui, ... }: {
    nixosConfigurations.devserver = nixpkgs.lib.nixosSystem {
      modules = [
        buzz-gpui.nixosModules.default
        {
          services.buzzd = {
            enable = true;
            user = "ben";
            listen = [ "tcp://127.0.0.1:7420" ];
          };
        }
      ];
    };
  };
}
```

- `buzzd` runs as the user you name. Its agents work in that user's files and
  use the harnesses and `buzz-acp` installed for that user. Add tools to
  `services.buzzd.path` if they are not on the user's login-shell PATH.
- Agent keys are stored as owner-only files in `~/.local/share/buzzd`.
- `buzzd` listens only on localhost. Make it reachable from the laptop with a
  Tailscale service, or any other tunnel, pointing at `127.0.0.1:7420`.
- Print the token that clients must present by running, as that user:

  ```sh
  buzzd token
  ```

  Without the module, run it with `nix run .#buzzd -- run`. It reads
  `~/.config/buzzd/config.toml`. Every setting is documented in
  `crates/buzzd/src/main.rs`.

Then in the app open **Agents → Add host**. Enter the address the laptop
reaches, for example `tcp://devserver.your-tailnet.ts.net:7420`, and the token.
The token is kept in the laptop's keyring.

## Development

```sh
nix develop                         # toolchain and native libraries
cargo run -p buzz-gpui              # the app
cargo run -p buzzd -- run           # an agent host
cargo test --workspace
```

- `scripts/test-relay.sh up && scripts/test-relay.sh seed` runs a throwaway
  upstream relay with seeded members, channels and messages. Set
  `BUZZ_TEST_RELAY=$PWD/.test-relay` to run the integration tests against it.
  See [scripts/TEST-RELAY.md](scripts/TEST-RELAY.md).
- [docs/AGENT-BRIEF.md](docs/AGENT-BRIEF.md) has the recipe for running the app
  headless and taking screenshots.
- `docs/port/relay.md` and `docs/port/ui.md` are the specifications of
  buzz-app's data layer and visual design that this port follows.
