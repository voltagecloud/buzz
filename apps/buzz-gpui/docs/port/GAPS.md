# Parity gaps with buzz-app

What buzz-app does that this port does not yet do, or does differently.
The goal is one-to-one parity. Six audits went through buzz-app area by area
on 2026-10-09 and checked each user-visible behaviour against the port. Each
row cites both codebases and carries a size: S is under a day, M is a few
days, and L is a week or more.

| Area | File | Missing | Partial | Different | Open rows |
|---|---|---:|---:|---:|---:|
| App shell, navigation, windows, updates, notifications | [gaps/shell.md](gaps/shell.md) | 47 | 12 | 12 | 71 |
| Settings, profile, identity | [gaps/settings.md](gaps/settings.md) | 50 | 14 | 28 | 92 |
| Messages, composer, media, search | [gaps/messages.md](gaps/messages.md) | 90 | 42 | 28 | 160 |
| Channels, communities, relay layer | [gaps/channels.md](gaps/channels.md) | 0 | 1 | 11 | 12 |
| Agents, sessions, workflows, terminal | [gaps/agents.md](gaps/agents.md) | 2 | 0 | 7 | 9 |
| Bundled plugins and the rest | [gaps/plugins.md](gaps/plugins.md) | 30 | 6 | 2 | 38 |

Open rows by size: about 805 small, 355 medium, and 75 large. Some
behaviours show up in more than one audit.

The rows marked IN PROGRESS (agent restart on save, provider tests and
Databricks fields, notification sounds, the private key widget, the
status calendar, the default profile, saved agent activity, leaving a
community) were done after the audits ran, as was live Markdown styling
in the composer, with its markers visible rather than hidden.

## buzz-app main of 2026-10-11

On 2026-10-11 the buzz-app fork was rebased onto buzz-app main at #833
(`d55d70a0`), and the port pins the result (fork rev `ec7d795f`). Each
upstream change since the previous base is listed below, with the area file
that holds its rows.

| Upstream change | Area | Status |
|---|---|---|
| #609 `08b3cbe8` share compact clock formatting | [messages](gaps/messages.md) | DONE |
| #818 `8820e5a5` bundle the Claude Code agent type by default | [plugins](gaps/plugins.md), [agents](gaps/agents.md) §30 | DONE |
| #788 `635b699c` placeholder Builderlab agent type in Agents2 | [agents](gaps/agents.md) §30 | SKIPPED (decided: Builderlab) |
| #751 `89ee72a9` native Codex agents with CLI defaults | [agents](gaps/agents.md) §30 | DONE |
| #769 `bd7797f7` bounded stdin for fixed plugin host commands | [agents](gaps/agents.md) §30 | N/A (external plugins only) |
| #813 `5608988d` Sonner for bottom-right app toasts | [shell](gaps/shell.md) §11 | DONE |
| #775 `8cef21d0` shared team instructions in their own record | [agents](gaps/agents.md) §30 | DONE |
| #824 `54bd0634` complete the Builderlab mock in a test | — | N/A (test only) |
| `b23fb875` Codex agents in Agents2 | [agents](gaps/agents.md) §30 | DONE |
| #790 `5fe10620` run each Agents2 agent in one copy of the app | [agents](gaps/agents.md) §30 | DONE |
| #819 `0217167f` inbox detail heading opens its context | [plugins](gaps/plugins.md) | DONE |
| #797 `2b079c66` test-audit agent skill | — | N/A (contributor docs) |
| #823 `63d2f5ee` rebuild Vite's dependency cache on dev start | — | N/A (web dev tooling) |
| #830 `fcb3c655` bundle the Codex agent type by default | [plugins](gaps/plugins.md), [agents](gaps/agents.md) §30 | DONE |
| #829 `829b1ecc` split an invitation paging test | — | N/A (test only) |
| #832 `f927c881` run bundled Codex with danger-full-access | [agents](gaps/agents.md) §30 | DONE |
| #826 `293c5191` view Codex and Claude sessions started in Buzz | [agents](gaps/agents.md) §30 | DONE (runtime naming); scripts N/A |
| #831 `f8705a95` default Codex workspace to `~/.buzz` | [agents](gaps/agents.md) §30 | DONE |
| #803 `f138a467` show typed angle-bracket text | [messages](gaps/messages.md) | DONE |
| #817 `82d165d5` Me sidebar working indicators and activity | [plugins](gaps/plugins.md) | DONE |
| #843 `35972bde` cancel toast exit-removal timers on unmount | [shell](gaps/shell.md) §11 | N/A (Sonner library fix) |
| #825 `a2b7db9a` nonmember mention prompt defaults to Invite | [messages](gaps/messages.md) | DONE |
| #798 `37dcd836` design-system chrome tabs for Me and Messages | [shell](gaps/shell.md) §11 | DONE |
| #851 `6460c4ee` Codex replies in the thread, not under the mention | [agents](gaps/agents.md) §30 | DONE |
| #833 `d55d70a0` commit attribution as Agents2 does it | [agents](gaps/agents.md) §30 | DONE |

## buzz-app main of 2026-10-14

On 2026-10-14 the buzz-app fork was rebased onto buzz-app main (`687985b5`,
#1044), and the port pins the result (fork rev `219fea82`). Upstream #859
removed v1 agents, which deleted the agent controller's runtime and the model
probes, harness installs and npm manager our extraction had moved into
buzz-desktop; those extraction commits were dropped, as were our two patches
to the deleted runtime. #961 changed only TypeScript, so a rebase was
practical. Each upstream change since #833 is listed below, with the area
file that holds its rows.

| Upstream change | Area | Status |
|---|---|---|
| #838 `02fbc69d` keep agents running across communities | [agents](gaps/agents.md) | DONE (ported ahead of merge; checked against the merged version) |
| #855 `ca078585` live agent session viewer and recorded context | — | N/A (developer scripts) |
| #856 `f9a5e6f5` choose model and effort for mentioned agents | [agents](gaps/agents.md) | DONE |
| #858 `63f7e903` bring existing agents into Agents2 | [agents](gaps/agents.md) | DONE |
| #859 `29c36a8b` remove v1 agents | [agents](gaps/agents.md), [settings](gaps/settings.md) | DONE (mission control rebased on Agents; DIFFERENT, ours) |
| #860 `ecfbb8a5` agents working: reactions, local typing | [agents](gaps/agents.md) | DONE (reactions later removed by #484) |
| #865 `7400d680` agents talk to each other, quiet lines | [agents](gaps/agents.md), [messages](gaps/messages.md) | DONE |
| #864 `6a3eae76` lift history with a growing composer | [messages](gaps/messages.md) | DONE (GPUI's list already holds the end) |
| #867 `146caad4` teams and channel templates | [agents](gaps/agents.md), [channels](gaps/channels.md) | DONE |
| #847 `a8a5ba15` more breathing room for messages | [messages](gaps/messages.md) | DONE |
| #871 `ea48cc8d` rename Agents2 to Agents | [agents](gaps/agents.md) | DONE (user-visible names, storage folders and settings move; Rust modules keep `agents2`) |
| #897 `a1d5788d` drop the box after a lone custom emoji | [messages](gaps/messages.md) | N/A (custom emoji are shortcode text in the composer) |
| #874 `81f8de7f` no deadline on an agent's work | [agents](gaps/agents.md) | DONE |
| #849 `8c151a23` compact icon actions | [shell](gaps/shell.md) | DONE |
| #840 `736975e2` buzz-mcp media_get | [agents](gaps/agents.md) | DONE (new host request) |
| #910 `94fdec47` wake on mentions only | [agents](gaps/agents.md) | DONE |
| #870 `287a557d` hosted agent types and the owner's agent list | [agents](gaps/agents.md) | DONE (owner's list; hosted types for plugins, docs/plugins.md); SKIPPED (Builderlab's own hosted type) |
| #850 `63c39eee` remove blank-tab creation from the side pane | [channels](gaps/channels.md) | DONE |
| #903 `6730a3d2` pick the agent session for session-debug | [agents](gaps/agents.md) | DONE (prompt part); scripts N/A |
| #912 `dd360ca3` re-sign stale agent removals | [agents](gaps/agents.md) | DONE (already signed fresh) |
| #913 `0cf13d71` Kenny's agent page | [agents](gaps/agents.md) | DONE; DIFFERENT (pack posters only, no CDN animations; host picker kept); type create-section API N/A (plugins being redone) |
| #904 `be861c9a` keep replies intact on same-thread links | [messages](gaps/messages.md) | DONE (our #853 port reconciled to it) |
| #882 `763c023e` offer the viewer's remote agents before they join | [messages](gaps/messages.md) | DONE |
| `af303dff` channel threads in main and side tabs | [channels](gaps/channels.md) | DONE (no keyboard tab moves: GPUI tabs take no focus) |
| `55b71bf1` default pane for new threads | [settings](gaps/settings.md) | DONE |
| #915 `c13b97fc` Builderlab as a hosted Agents type | — | SKIPPED (decided: Builderlab) |
| `ebadc94c` drag thread tabs between panes | [channels](gaps/channels.md) | DONE |
| #916 `544d0cd5` old Builderlab Settings | — | SKIPPED (decided: Builderlab) |
| `0de0f26f` highlight a message when its thread closes | [messages](gaps/messages.md) | DONE |
| #924 `350f7d71` call the agent type "Claude" | [agents](gaps/agents.md) | DONE |
| #909 `c5c2719d` resume typing and focus across tabs | [channels](gaps/channels.md) | DONE |
| #927 `03b73ec5` corrected thread links stay in their pane | [channels](gaps/channels.md) | DONE |
| #484 `f93c3378` agent progress across channels, threads and messages | [agents](gaps/agents.md) | DONE; DIFFERENT details (step list as a hover tooltip, no height animation) |
| #923 `f1fee808` tooltip "Emoji and GIFS" | [messages](gaps/messages.md) | DONE |
| #925 `50b43239` emoji and GIF picker flash | — | N/A (GPUI draws only the selected tab) |
| #919 `c3f6068e` Codex steering hold | [agents](gaps/agents.md) | DONE |
| #877 `88575c00` pull request icon flash | [messages](gaps/messages.md) | DONE |
| #806 `ddc01962` an agent reads and writes its attention | [agents](gaps/agents.md) | DONE |
| #807 `ebd529e8` buzz-mcp attention tools | [agents](gaps/agents.md) | DONE |
| #809 `017ec30c` wake Claude for watches and timers | [agents](gaps/agents.md) | DONE |
| #951 `76c1d9f1` attention off until the owner turns it on | [agents](gaps/agents.md) | DONE |
| #740 `d85e78e2` submenus open at once on hover | [shell](gaps/shell.md) | DONE (already so) |
| #896 `fb22f5ff` cached native portraits | — | N/A (WebKit workaround; GPUI's asset cache keeps avatars) |
| #842 `fc8d324f` Codex and Claude session transcripts | [agents](gaps/agents.md) | DONE |
| #931 `b1377049` picture of a hosted agent | — | SKIPPED (decided: Builderlab) |
| #970 `ef824638` verified relay workflow mentions | [agents](gaps/agents.md) | DONE |
| #854 `5fd9a29b` round the macOS app icon | — | N/A (no macOS bundle icon from buzz-app) |
| #900 `df03af99` templates pick individual agents | [channels](gaps/channels.md) | DONE |
| #943, #939, #937, #938 React render performance | — | N/A |
| #848 `96f1c215` Messages selected across sidebar destinations | [shell](gaps/shell.md) | DONE (ported ahead of merge; matches) |
| #920 `ed62957c` PR links the pane cannot show | [plugins](gaps/plugins.md) | DONE |
| #921 `1e1ad0b8` Retry reloads the agent list | [agents](gaps/agents.md) | DONE |
| #930 `99ec6341` neutral thread placeholder | [messages](gaps/messages.md) | DONE |
| #934 `96071018` focus a moved thread tab | [channels](gaps/channels.md) | DONE (drag only) |
| #889 `d6bc96e0` Codex queues a differing pick | [agents](gaps/agents.md) | DONE |
| #815 `fa3b7208` editing in Inbox details | [plugins](gaps/plugins.md) | DONE |
| #948 `bc6e9ed1` attachment previews and eager uploads | [messages](gaps/messages.md) | DONE (no zoom in the draft viewer) |
| #935 `bef8e4d7` exact member name on Space | [messages](gaps/messages.md) | DONE |
| #845 `e68f8787` sidebar collapse on all pages | [shell](gaps/shell.md) | DONE |
| #958 `0b68a181` channel type from the first t tag | [channels](gaps/channels.md) | DONE |
| #846 `d23a46e1` the clicked conversation's session in agent tabs | [agents](gaps/agents.md) | DONE; DIFFERENT (transcript replaces the tab content) |
| #878 `b351b880` log a failed Codex terminal clean | [agents](gaps/agents.md) | DONE |
| #887 `d01e2ff5` picks return with a recovered send | [messages](gaps/messages.md) | DONE |
| #932 `2ad929e5` "Soft & wide" agent mark | [shell](gaps/shell.md) | DONE |
| #988 `d910a268` remove personal groups | [channels](gaps/channels.md), [settings](gaps/settings.md) | DONE |
| #984 `f208fa61` Codex gets the base prompt | [agents](gaps/agents.md) | DONE |
| #835 `ad4040de` IFC policy library | — | N/A (library crates only) |
| #952 `7503384d` providers, models and agent migrations | [agents](gaps/agents.md) | DONE (new host request `SetType`); DIFFERENT (Claude/Codex status and sessions stay tabs, not "Runtime diagnostics") |
| #991 `09341386` softer compact dropdown outlines | [shell](gaps/shell.md) | DONE |
| #982 `33b0e15f` restore agent memory | [agents](gaps/agents.md) | DONE |
| #949 `75c501b3` Buzz's own copies of Claude and Codex | [agents](gaps/agents.md) | DONE; DIFFERENT (scripts pinned in the host's declared processes, one copy per host, device-code Codex sign-in on remote hosts) |
| #996 `0aa6d9f4` websocket recovery across rollouts | [channels](gaps/channels.md) | DONE |
| #1003 `e7484702` copy the pairing code | [settings](gaps/settings.md) | DONE |
| #1006 `510c372c` wake reading after an exact reveal | [messages](gaps/messages.md) | DONE (reading runs every frame) |
| #801 `2beca5b3` workspace surfaces and split controls | [shell](gaps/shell.md) | DONE; emoji composition N/A (shortcode text) |
| #936 `b4331d7a` Ctrl+W closes tabs; Ctrl+Tab | [shell](gaps/shell.md) | DONE (Ctrl+Q quit kept) |
| #1007 `1aa3a97b` video fullscreen button above inline controls | — | N/A (already drawn last) |
| #1004 `4c1abff7` automatic updates for Windows/Linux previews | [shell](gaps/shell.md) | DIFFERENT (our updates come from our GitHub releases) |
| #1000 `edf8ee58` Me chats as one top-level conversation | [agents](gaps/agents.md) | DONE |
| #1008 `45e8b536` shared base prompt and core memory reads | [agents](gaps/agents.md) | DONE |
| #828 `02512e64` join previews | [channels](gaps/channels.md) | DONE |
| #677 `2caaffe2` Inbox archive and reopen | [plugins](gaps/plugins.md) | DONE (archive button on hover only) |
| #998 `979a2907` agents typing to everyone | [agents](gaps/agents.md) | DONE |
| #978 `5540d835` Builderlab key link | — | SKIPPED (decided: Builderlab) |
| #945 `6aea0271` sidebar in place for the Personal space | [shell](gaps/shell.md) | DONE |
| #862 `dfbb239b` agents online | [agents](gaps/agents.md) | DONE |
| #886 `17e88086` full sent images and GIFs | [messages](gaps/messages.md) | DONE |
| #1018 `49ae64bb` push main-pane threads, loading mark | [channels](gaps/channels.md) | DONE |
| #1039 `52cdd382` bottom-follow fixes | [messages](gaps/messages.md) | DONE (already so); flake parts N/A |
| #990 `bcec65b4` desktop animated avatars | [messages](gaps/messages.md) | DONE (still frame); broker part N/A |
| #961 `3c2d4cfa` separate the agent runner from the desktop app | — | N/A (plugins being redone; no behaviour change) |
| #964 `9e51c0fe` channel labels | [channels](gaps/channels.md) | DONE (data layer; no UI upstream either) |
| #1068 `506ac5d4` JPEG uploads and control overlap | [messages](gaps/messages.md) | DONE |
| #1013 `f442ed0a` agent avatars in the template editor | [channels](gaps/channels.md) | DONE |
| #1055 `80d0161d` buzz-mcp channel and member tools | [agents](gaps/agents.md) | DONE |
| #995 `71e2c229` native nsec backup HTTP client | — | N/A (library crate; nothing uses it yet) |
| #1085 `1f4cba4d` keep following after a notification reveal | [messages](gaps/messages.md) | DONE (already so) |
| #1087 `01830334` notification-reveal flakes | — | N/A (test only; behaviour already so) |
| #1059 `15e21f50` Codex instructions without truncation | [agents](gaps/agents.md) | DONE |
| #1005 `2e004856` Me conversations like project chats | [plugins](gaps/plugins.md) | DONE; DIFFERENT (plain assistant menu, no model/effort controls in Me) |
| #1047 `e85c5c45` GTK font hinting | — | N/A (tunes WebKitGTK; GPUI renders its own text) |
| #1038 `3be30942` broker request bodies | — | N/A (dev browser broker) |
| #1030 `897caf35` reload the recorded plugin source | — | N/A (plugins being redone) |
| #1086 `51b8d969` every model Claude lists | [agents](gaps/agents.md) | DONE |
| #1044 `687985b5` search again for people added later | — | N/A (no session people cache; each visit searches) |
| #1001 `06ea6074` Vite watcher | — | N/A (web tooling) |
| #880 `4bb08a1f` member-search debounce on a controlled clock | — | N/A (test only) |
| #911 `fa03be63` four CI flakes | — | N/A (test only) |
| #917 `bd52c734` mute-glide check timing | — | N/A (test only) |
| #933 `bb1f8d6e` flaky member rejection test | — | N/A (test only) |
| #918 `be71739d` sidenav avatar measurement | — | N/A (test only) |
| #963 `53970a71` composer toolbar fade-in wait | — | N/A (test only) |
| #888 `35569021` inbox exact-target reveal wait | — | N/A (test only) |
| #892 `2161e85a` workflows condition change event | — | N/A (test only) |
| #980 `06b43c1d` local activity test binding | — | N/A (test only) |
| #981 `0ec0c1ad` keyboard retry session gate | — | N/A (test only) |
| #890 `305f1066` repeat changed tests 10x in CI | — | N/A (CI) |
| #1002 `d1af29a9` six cross-branch CI flakes | — | N/A (test only) |
| #1020 `2fa03e8e` held sidebar preferences read | — | N/A (test only) |
| #1019 `bfafac16` agent activity popup focus wait | — | N/A (test only) |
| #999 `b17e891d` voice-note upload fixture | — | N/A (test only) |
| #997 `462f3473` inbox edit journey | — | N/A (test only) |
| #989 `25e996d6` browser journeys in nine shards | — | N/A (CI) |
| #1071 `11251bcb` WebKit trace screenshots | — | N/A (test only) |
| #891 `fb9cad1a` close the activity popover through Base UI actions | — | N/A (Base UI library behaviour; GPUI popovers close directly) |

## Decided

- **Skipped:**
  - buzz-app's external `plugin.js` plugins, their signing and host
    grants. Bundled plugins are ported natively instead, and people build
    their own as programs ([docs/plugins.md](../plugins.md)).
  - Block-hosted services: Hosted communities, Builderlab and the enterprise
    login gate.
- **Updates** come from GitHub releases. Copies installed from the .deb
  update themselves; macOS, Windows and development builds only say a
  release is out (`crates/app/src/updates/`).
- **Reuse buzz-app's Rust where it exists:**
  - The chat data layer is TypeScript in buzz-app, so `crates/relay` stays
    our Rust port of it.
  - Native services move from the fork's `src-tauri` into a library one at a
    time, as their features are built: media conversion, the upload spool,
    the PTY backend, OAuth, sign-out and the plugin records. The v1 model
    catalogs and harness installs went with upstream's v1 agents.
- **Plugin system:** being redone, so upstream changes to the plugin system
  itself are N/A until then.

## The large pieces

- **Bundled plugins:** Me, Inbox, Reminders, Projects, GitHub, Workflows,
  Terminal, Templates and teams, Hosted communities, Builderlab, Pair mobile,
  Channel usage and Community admin. buzz-app turns all of these on by
  default except Todos, Templates and teams, and Bestie. Each has pages,
  panels, Settings cards and relay records.
- **Plugin system:** a registry for pages, panels, message, inline and link
  renderers, and Settings cards. Also external plugin import with signing and
  host grants. This port compiles plugins in, so the registry becomes
  internal extension points.
- **Workspace shell:**
  - primary pages as sidebar rows;
  - tabbed channel panels with secondary conversation and thread tabs, and a
    New tab picker;
  - a companion dock;
  - a resizable sidebar;
  - a narrow-window drawer.
- **Relay reliability:** a disk cache of channel heads and profiles, and an
  offline resume that shows saved conversations.
- **Media:** HEIC to JPEG and video to MP4 conversion before upload. Uploads
  are deferred until Send.
- **Composer:** WYSIWYG formatting with no visible Markdown markers, and
  saved teams in mentions.
- **Agents:**
  - the full inventory, import, clone and identity actions;
  - the agent catalog;
  - team snapshots and import;
  - Agents2 and its agent types;
  - sessions, with history, sharing and a runtime;
  - observer telemetry;
  - pairing and remote agent setup;
  - the Pi installer;
  - Databricks browser sign-in.
- **Native extras:** a terminal (PTY) panel per channel, and the in-app
  browser.

## Not applicable

Each audit ends with what doesn't apply to a native Linux app:
- macOS and Windows window behaviour;
- webview-only and dev-server-only behaviour;
- behaviour of buzz-app's JavaScript plugin runtime itself.
