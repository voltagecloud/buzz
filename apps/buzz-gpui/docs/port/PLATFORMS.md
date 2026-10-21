# Platform parity

Linux is the full-featured target. This file lists every place where the
app behaves differently on macOS or Windows, what Linux does there, and
what the other two do now. It was written from an audit of the code on
2026-10-11: every `cfg(target_os = …)`, `cfg(unix)` and `cfg(not(…))`
stub, every no-op fallback, the UI and docs that said "Linux only", and
the notes of the change that first built the app on macOS and Windows.

Status:

- **DONE**: the feature works natively on that platform.
- **DIFFERENT**: it works differently or not at all, for the reason
  given. Nothing is faked.

macOS and Windows were checked by CI builds and lints only. The last
section lists what a person still has to try on real machines.

## App shell

| Feature | Linux | macOS | Windows |
|---|---|---|---|
| Credentials (human key, agent keys, host tokens) | Secret Service through buzz-credential-store | DONE: login keychain, generic passwords (`crates/keychain`) | DONE: Credential Manager through buzz-credential-store's `keyring` backend (`windows-native`), as buzz-app does; create-only writes lock under `%USERPROFILE%\.buzz-foundation` |
| One running app; a second launch hands its link over and exits | Unix socket in `$XDG_RUNTIME_DIR` (`single_instance.rs`) | DONE: Launch Services keeps one app and sends links as open-URL events; the Unix socket (in `$TMPDIR`) covers launches outside Launch Services | DONE: a named pipe per user and profile |
| `buzz://` links from the OS | `Exec=buzz-gpui %U`, read from argv | DONE: links arrive as open-URL events (`App::on_open_urls`), also on a cold start | DONE: argv, as on Linux |
| `buzz://` scheme registration | `.desktop` file; portable release builds write a user handler (`deep_links::register_scheme`) | DONE: `CFBundleURLTypes` in Info.plist; Launch Services registers the app when it is copied to Applications | DONE: release builds write `HKCU\Software\Classes\buzz` at launch, since the zip has no installer |
| Desktop notifications | freedesktop notification service over D-Bus (`notifications/banners.rs`) | DONE: GPUI's `UNUserNotificationCenter` backend (`notifications/system.rs`); macOS asks for permission on the first banner. DIFFERENT: none outside `Buzz.app`, which the framework requires | DONE: GPUI's WinRT toast backend, under the app's AppUserModelID, registered at launch |
| Clicking a notification opens its message | `ActionInvoked` plus an xdg-activation token | DONE: GPUI notification response, by the banner's tag | DONE: the same, while Buzz runs. DIFFERENT: a toast clicked in the Action Center after Buzz quit does not start it; that needs a COM activator an installer registers |
| Notification service status in Settings | probes the D-Bus service | DONE: available inside `Buzz.app` | DONE: always available |
| Bringing the window forward (hand-off, tray, notification) | xdg-activation token (`gpui_linux::set_activation_token`) | DONE: the app is activated with the window | DONE: GPUI's activation takes the foreground |
| Tray icon: open, quit, unread attention, minimize or close to tray | StatusNotifierItem over D-Bus (`tray/linux.rs`) | DONE: menu-bar item through `tray-icon` (`tray/native.rs`); a click opens its menu, as on every menu-bar item | DONE: notification-area icon through `tray-icon`; a click opens Buzz, a right-click the menu |
| Dock icon click reopens a hidden window | n/a | DONE: `App::on_reopen` | n/a |
| Quit shortcut | none (Ctrl+Q is reserved) | DONE: app menu with Quit Buzz (⌘Q) | n/a |
| Window chrome | client-side: the header is the title bar, with Buzz's own minimize, maximize and close | DONE: transparent title bar with the system's window buttons at buzz-app's position, and the header inset 80 px for them | DONE: no system caption; Buzz's own buttons, and the header is the caption in the system's hit test (drag, double-click, right-click menu, snapping). DIFFERENT: no Snap Layouts flyout on the maximize button, which would hand its clicks to the system |
| App icon | hicolor PNGs from the .deb | DONE: `.icns` in the bundle | DONE: icon resource in the .exe (window, taskbar, Explorer) |
| Launch at login | none | n/a: Linux has no such setting either | n/a |
| Window position and size across launches | GPUI window bounds | DONE | DONE |
| Machine idle for automatic away | GNOME Mutter idle monitor | DONE: `user-idle`, as buzz-app | DONE: `user-idle`, as buzz-app |
| Network back online: reconnect communities | default route in `/proc/net/route` | DONE: asks for a route to the internet by connecting a UDP socket, which sends nothing | DONE: same probe |
| Copy image | `arboard` (GPUI's clipboard has text only on Wayland) | DONE: `arboard` | DONE: `arboard` |
| Paste and drop files | vendored GPUI patch for `text/uri-list` | DONE: GPUI reads file names from the pasteboard | DONE: GPUI reads `CF_HDROP` |
| Time zone abbreviation in times | `localtime_r` | DONE | DIFFERENT: Windows names zones in full ("Pacific Standard Time"), so times show a GMT offset |
| HEIC avatars through ffmpeg | `ffmpeg` from the package's dependencies | DIFFERENT: needs `brew install ffmpeg` | DIFFERENT: ffmpeg must be on `PATH`; the zip does not carry it. It runs without a console window |
| Media playback and sounds | GStreamer from the distribution | DONE: GStreamer bundled in Buzz.app | DONE: GStreamer bundled in the zip |
| Data and config locations | XDG directories | DONE: `~/Library/Application Support` | DONE: `%APPDATA%` |

## Agents and hosts

| Feature | Linux | macOS | Windows |
|---|---|---|---|
| Embedded agent host | in-process host (`crates/host`) | DONE | DONE |
| Agent processes and their descendants | buzz-agent-controller, process groups | DONE | DONE: buzz-agent-controller uses job objects |
| Refuse to start agents while old Buzz runs | `ps -axo pid=,comm=` | DONE | DONE: `tasklist`; before, `ps` did not exist there, so every agent start was refused |
| Agent-type processes (`crates/host/src/processes.rs`) | own process group; SIGTERM, then SIGKILL to the group | DONE | DONE: a job object (buzz-desktop's) ends descendants too; no console window |
| Channel terminals | login shell in a PTY (`buzz_desktop::terminal`) | DONE | DIFFERENT: buzz-desktop's terminal is Unix-only ("Local terminals require macOS or Linux"); buzz-app has none on Windows either. A ConPTY backend belongs in buzz-desktop |
| Installing Pi and Claude Code into Buzz's tools | `buzz_desktop::harness_setup` | DONE | DIFFERENT: buzz-desktop supports macOS and Linux only, as buzz-app; install them yourself and Buzz finds them on `PATH` |
| Remote hosts over `tcp://` | yes | DONE | DONE |
| Remote hosts and `buzzd` over `unix://` | Unix sockets | DONE | DIFFERENT: tokio has no Unix sockets on Windows; use `tcp://` |
| Owner-only files (config, `buzzd` token, agent key files) | modes 0600 and 0700 | DONE | DIFFERENT: no Unix modes; the files are in the user's profile, whose ACL already admits only the user, SYSTEM and Administrators |
| `buzzd` stops cleanly when the service manager stops it | SIGTERM | DONE | DONE: Ctrl+Break, console close, logoff and shutdown events |
| `buzzd` host name | `/etc/hostname` | DONE: the system's host name, without `.local` (macOS has no `/etc/hostname`) | DONE: the system's host name |

## Packaging

| Item | Linux | macOS | Windows |
|---|---|---|---|
| Package | `.deb` | `.dmg` with `Buzz.app` | `.zip` |
| Signing | n/a | DIFFERENT: ad-hoc signature only; Developer ID signing and notarisation need Apple credentials the project does not have, so Gatekeeper warns on first open | DIFFERENT: unsigned; signing needs a code-signing certificate, so SmartScreen warns |
| Installer, Start menu entry | n/a | n/a | DIFFERENT: the zip has no installer and adds no Start menu entry; registry entries for `buzz://` and notifications are written at launch instead |

## Smoke tests on real machines

Nothing in this file was run on a Mac or a Windows PC. CI builds, lints
and packages both. Someone with the hardware should check:

- **Both:** sign in with a key already in the keychain or Credential
  Manager; a second launch and a `buzz://` link from a browser reach the
  running app and bring its window forward; a mention shows a banner and
  clicking it opens the message; the tray icon opens and quits Buzz, gets
  its dot for unread mentions, and close and minimize to tray hide the
  window; Copy image in the image viewer; away after ten minutes idle
  while typing elsewhere keeps you online; pulling the network cable and
  plugging it back reconnects at once; starting an agent.
- **macOS:** the window buttons sit inside the header, centred on it,
  clear of the back and forward buttons; the header drags the window and
  double-clicking it zooms; ⌘Q quits; a Dock click reopens a window
  hidden to the menu bar; the first banner asks for permission.
- **Windows:** the taskbar, window and Explorer show the Buzz icon; the
  header drags the window, double-click maximizes, dragging to the screen
  edge snaps, and the buttons in it still click; toasts show "Buzz";
  stopping a Claude Code process ends what it started; `buzzd` stops its
  agents when its console closes; no console windows flash. Version
  probes run through buzz-desktop's `process::run_output` start without
  `CREATE_NO_WINDOW`; if a console flashes when Settings lists
  harnesses, the fix belongs in buzz-desktop.
