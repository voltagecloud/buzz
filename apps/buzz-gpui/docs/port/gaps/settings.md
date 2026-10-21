# Parity audit: Settings, account and identity

Reference: `/home/ben/projects/buzz-app` (cited as `buzz-app:src/...`).
Port: `/home/ben/projects/buzz-gpui` main checkout (cited relative to
`crates/app/src/` unless another crate is named). Worktrees were not read.

Statuses: MISSING, PARTIAL (what is missing is stated), DIFFERENT (how is
stated), DONE (with the files), SKIPPED (decided: one of the decisions in
GAPS.md).

Line numbers in buzz-app `src/app/*` files are for the file itself.

---

## 1. Settings shell (Settings.tsx, SettingsSidebar.tsx, settings-sections.ts)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Updates" section in the App group ("Software updates": version, status text, Check for updates / Update now / Retry) | `src/app/settings-sections.ts:48`; `src/app/Settings.tsx:177-182` (renders `features/updates/UpdateSettings.tsx`) | DONE (ui/settings/updates.rs: this build has no updater, so a check ends in buzz-app's "unavailable on this build" state; the updater is deferred per GAPS.md) | Add an Updates section and an updater backend for Linux packages. The section contents belong to the updates audit. | M |
| Plugin-card nav entries: community group "Templates" and "Personal groups"; "Administration → Membership" (only for owner/admin, by card visibility); "Integrations → Builderlab"; "Account → Pair mobile"; "Communities → Hosted communities" | `src/app/settings-sections.ts:62-127`; `src/features/settings/service.ts:50-69,93-105` (visibility gating) | DONE (crates/app/src/ui/settings/cards.rs, crates/app/src/ui/settings/personal_groups.rs, crates/app/src/ui/settings/pairing/): Builderlab and Hosted communities are skipped by decision | - | L |
| "Communities" section (saved communities, Selected badge, Leave) | No equivalent page. buzz-app's "Communities" group holds only the Hosted plugin card (`settings-sections.ts:106-112`). | DONE (port addition, kept: the rail menu's Leave lives here until the rail menu exists) | Decide whether to keep it. If kept, it is an addition to the reference. | S |
| Leave a community end to end | Community rail menu in buzz-app (outside this slice) | DONE (ui/settings/communities.rs, community.rs) | Another agent is adding it. | — |
| "About" section (version, license, notices) | None (version is shown on the Updates page) | DONE (port addition, kept for the license notices; the version is also on Updates) | Keep as an addition, or fold the version into Updates. | S |
| Settings sidebar width follows the saved channel-sidebar width | `src/app/SettingsSidebar.tsx:27` (`readChannelSidebarWidth`) | DONE (both are 260px; the Settings sidebar follows the saved width once the channel sidebar can be resized, shell.md §2) | Read the persisted channel-sidebar width (default 260, minimum 220). | S |
| Fall back to the default section when the selected one disappears (for example, a community card when the community is deselected) | `src/app/Settings.tsx:86-95` | DONE (ui/settings/mod.rs `follow_community`) | Reset `section` to the default when it is no longer listed. | S |
| Open Settings at a requested section from anywhere and report completion (navigation target `{kind:"settings", section}`) | `src/app/Settings.tsx:81-100` | DONE (ui/navigation.rs `Place::Settings`: any caller navigates to a section) | A general "open Settings at section X" action for deep links and the community rail ("Community settings"). | S |

## 2. Profile (ProfileSettings.tsx, features/profiles)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Profile with no community selected edits the app-wide default profile ("Used for new communities…") | `src/app/ProfileSettings.tsx:113-116,159-165` | DONE (`ui/settings/profile/mod.rs`) | Another agent is adding it. | — |
| Saving a community profile also updates the local default profile (`profileDefault`) | `src/app/ProfileSettings.tsx:281-288` | DONE (`ui/settings/profile/mod.rs`) | Another agent is adding it. | — |
| Profile load states: "Loading your profile in {community}…", load error with "Retry loading profile", "Community profiles are not available in this build yet" | `src/app/ProfileSettings.tsx:106-135,174-196` | DONE (`ui/settings/profile/mod.rs`; fresh read in `crates/relay/src/session/account.rs`). "Not available in this build" never applies: the relay is always available here. | A fresh read of the viewer's kind 0 on open, with loading, error and retry states. | M |
| Identity-state copy: "Opening your local identity…" and "Your local identity is unavailable. Connect your identity to edit your profile." | `src/app/ProfileSettings.tsx:168-173` | DONE (`ui/settings/profile/mod.rs`) | Match the two messages. | S |
| After publishing, re-read and confirm; error "Your profile change is not current. Your edits are retained; save again to retry." | `src/app/ProfileSettings.tsx:262-279` | DONE (`ui/settings/profile/mod.rs`) | Optional: add the confirming read and its error. | S |
| Avatar editor popover ("Edit avatar") with Image / Emoji / Background tabs, a dropzone ("Drop or browse"), a picture-URL input in the popover (Enter applies) and a Done button | `src/features/profiles/AvatarEditor.tsx:87-190,334-400,480-503` | DONE (`ui/settings/avatar/editor.rs`, `ui/settings/profile/mod.rs`) | A popover editor with the three tabs and a Done/commit step. | M |
| Drag and drop an image onto the avatar picker | `src/features/profiles/AvatarEditor.tsx:299-331` | DONE (`ui/settings/avatar/editor.rs`): the drop lands on the picker, as in buzz-app. Not exercised under Xvfb, which has no drag source. | File-drop handling on the picker. | S |
| Emoji avatar: pick an emoji, render it centered on a 512 px square with a background colour, upload it as PNG | `src/features/profiles/AvatarEditor.tsx:409-418,515-634`; `src/features/profiles/avatar-upload.ts:63-125` | DONE (`ui/settings/avatar/raster.rs`, `ui/settings/avatar/editor.rs`): drawn with cosmic-text from the bundled emoji font. The avatar emoji grid has search, skin tones, categories and a "Frequently used" row from reaction use. | Emoji picker tab, rasteriser (emoji font to PNG, trimmed and centered), upload. | M |
| Background swatches (23 colours) and a custom HSV colour picker | `src/features/profiles/AvatarEditor.tsx:46-70,419-460`; `src/features/profiles/AvatarCustomColor.tsx:1-149` | DONE (`ui/settings/avatar/editor.rs`, `ui/settings/avatar/raster.rs`) | Swatch grid and a custom colour picker. | S |
| "Remove avatar" button | `src/features/profiles/AvatarEditor.tsx:480-488` | DONE (`ui/settings/avatar/editor.rs`) | Add a destructive Remove avatar action. | S |
| Edit badge shows a pencil when an avatar is set, a plus when empty; picture error shown under the avatar | `src/features/profiles/AvatarEditor.tsx:126-160` | DONE (`ui/settings/profile/mod.rs`) | Switch the icon by state. | S |
| Accepts HEIC/HEIF in addition to PNG/JPEG/GIF/WebP | `src/features/profiles/avatar-upload.ts:44-45` | DONE (`ui/settings/avatar/mod.rs`): converted to JPEG with ffmpeg 8 or newer and the same arguments as buzz-app's `media_preparation.rs`. | HEIC/HEIF decode or convert before upload. | S |
| Picture URL lives inside the avatar popover, not as a form field | `src/features/profiles/AvatarEditor.tsx:382-404`; `src/features/communities/ProfileFields.tsx` (fields are name and description only) | DONE (`ui/settings/avatar/editor.rs`) | Move it into the avatar editor. | S |
| Copy feedback: "Public key copied." / "Nostr address copied."; failure toast "Identity wasn’t copied" | `src/app/ProfileSettings.tsx:141-155,420-434` | DONE (`ui/settings/profile/mod.rs`). GPUI reports no failed write, so a differing read-back counts as the failure. | Match the wording. | S |
| Private key widget (Reveal/Hide, Copy, Cancel, masked field, cleared on blur or hide, messages) | `src/features/identity/PrivateKey.tsx:1-112`; `src/app/ProfileSettings.tsx:395-400` | DONE (`ui/settings/profile/private_key.rs`). Revealing reads the key in memory, so there is no busy state and no Cancel. | Another agent is adding it. | — |
| "Sign out of Buzz" row description: "Remove your private key from this device. Also ends Builderlab and hosted community logins." | `src/app/ProfileSettings.tsx:401-411` | DONE (`ui/settings/profile/mod.rs`): the first sentence only, since Builderlab and hosted communities are skipped. | Align the copy, or keep it deliberately because the keyring is shared. | S |

## 3. Sign out (features/identity/SignOut.tsx, src-tauri/src/sign_out.rs)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Private key widget inside the sign-out dialog | `src/features/identity/SignOut.tsx:97` | DONE (`ui/settings/profile/sign_out.rs`) | Another agent is adding it. | — |
| Confirm is gated: the key must be revealed or copied first, then the "I have my key" checkbox enables Sign out | `src/features/identity/SignOut.tsx:12-23,98-104` | DONE (`ui/settings/profile/sign_out.rs`) | The checkbox and the gating, wired to the private-key widget's interaction callback. | S |
| "Also erase everything else Buzz stores on this device": typed phrase "erase all my data", button becomes "Sign out and erase", explanation text | `src/features/identity/SignOut.tsx:9,105-153`; `src-tauri/src/sign_out.rs:97-150,435-530` | DONE (`sign_out.rs`, `ui/settings/profile/sign_out.rs`): erases the data, local data and cache folders, never through a link, trash first. | Wipe the data dir, caches and SQLite store safely: refuse to follow symlinks, move to trash first, then delete. | L |
| "Also remove my agents" (only with erase); otherwise kept agents keep `agents.json` / `defaults.json` and keys | `src/features/identity/SignOut.tsx:122-127,138-145`; `src-tauri/src/sign_out.rs:35-43` | DONE (`sign_out.rs`): kept agents keep `host/agent-controller/{agents,defaults}.json` and, with file credentials, `agent-keys`. | Remove the agent registry and agent keys from the embedded host when asked. | M |
| Wipe refusal check (development builds, linked storage folders, env overrides); shown as a disabled checkbox with the reason | `src/features/identity/SignOut.tsx:52-64,117-119`; `src-tauri/src/sign_out.rs:716-757` | DONE (`sign_out.rs`): development builds refuse, as they share the installed app's folders and agent keys. buzz-app's env overrides have no counterpart here. | Native refusal query and UI. | S |
| Dialog copy: "Signing out removes your private key from this device. Your key is how you sign in…" and the keep-data note "Your settings, agents and other Buzz data stay on this device…" | `src/features/identity/SignOut.tsx:78-80,148-153` | DONE (`ui/settings/profile/sign_out.rs`) | Align the copy. | S |
| Sign out stops this key's agents ("Agents start only for this key") | `src/features/identity/service.ts:76-78`; `src-tauri/src/sign_out.rs:759-800` | DONE (`sign_out.rs`; `crates/host/src/lib.rs`): agents stop before the restart, and the embedded host starts only agents the signed-in key authorized (`check_owner`). | Stop the local host's agents on sign-out, and start only the agents of the signed-in key. | M |
| Two-phase sign-out: write a marker, restart, delete at the next launch before any window opens, retry on failure and keep a finished record | `src-tauri/src/sign_out.rs:1-8,281-385,532-640` | DONE (`sign_out.rs`, one call in `main.rs`): ported from buzz-app's `sign_out.rs`, to move to the fork's native library later. | Acceptable if no wipe is offered. A wipe needs the marker-based flow. | M |
| Refuse while another Buzz using this sign-in is open ("Quit every other Buzz window using this sign-in…"); key-store instance lock shared across copies, including buzz-app | `src-tauri/src/sign_out.rs:19-34,93-95,186-250` | DONE (`sign_out.rs`): holds and honours buzz-app's key and all-Buzz instance locks and its finished record. | Take or honour buzz-app's key lock (`.dev.local.buzz.foundation.identity.instance.lock` in the data dir), or at least detect a running buzz-app. | M |

## 4. Identity setup and native identity (IdentitySetup.tsx, identity/service.ts, src-tauri/src/identity.rs)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Async restore with "Opening your saved identity…" | `src/features/identity/IdentitySetup.tsx:39-40`; `src/features/identity/service.ts:25-60` | DONE (`community.rs`, `ui/onboarding.rs`) | Optional: move the read off the UI thread and show the loading state. | S |
| Storage error shows the message and "Retry secure storage access" only | `src/features/identity/IdentitySetup.tsx:41-47`; `src/features/identity/service.ts:70` | DONE (`ui/onboarding.rs`) | A retry button that re-reads the keyring. Hide import/create while the read is failing. | S |
| Two-step layout: "Use an existing key" (primary) / "Create a new identity", then an import form with "Use this key" / "Back"; title "Your Buzz identity"; intro copy | `src/features/identity/IdentitySetup.tsx:36-118` | DONE (`ui/onboarding.rs`) | Match the flow and copy. | S |
| nsec field is a password input (no autocomplete or spellcheck), cleared on submit; helper "Copy this from Identity details in your old Buzz app. Never share it with anyone." | `src/features/identity/IdentitySetup.tsx:59-81` | DONE (`ui/onboarding.rs`) | Mask the field, clear it on submit, add the helper text. | S |
| Busy state: buttons disabled and "Saving your identity securely…" | `src/features/identity/IdentitySetup.tsx:83-119` | DONE (`ui/onboarding.rs`) | Run create/import in the background with a busy state. | S |
| Storage error wording (for example "Unlock your credential store and retry without changing keys."; the long MALFORMED text) | `src-tauri/src/identity.rs:17-18,159-169,214-225` | DONE (`crates/identity/src/lib.rs`) | Align the messages. | S |
| Debug builds use a separate keyring service | `src-tauri/src/identity.rs` via `buzz_credential_store::HUMAN_SERVICE` | DONE (deliberate): the port always uses the release service (`crates/identity/src/lib.rs`) | None. This is documented. | S |

## 5. Appearance (AppearanceSettings.tsx)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Save-failure toasts: "Appearance wasn’t saved" / "Interface size wasn’t saved", each with a Retry button | `src/app/AppearanceSettings.tsx:104-128` | DONE (appearance.rs, ui/settings/appearance.rs) | Surface the store errors as toasts with retry. | S |
| System tray rows (Show in tray, Minimize to tray, Close to tray) | None | DONE (port addition, kept) | Port-only addition. Keep it. | S |

## 6. Notifications (NotificationSettings.tsx)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Preference scope: one account-local set (`buzz-notification-preferences.v1`, per viewer) | `src/features/notifications/preferences.ts:8-28,76+` | DONE (notifications.rs, ui/settings/notifications.rs) | Store per viewer, and show the page without a community. | M |
| Category switches stay enabled when Desktop alerts is off | `src/app/NotificationSettings.tsx:168-187` | DONE (ui/settings/notifications.rs) | Leave them enabled. | S |
| "Notify while viewing" switch ("Include direct messages in the conversation you’re viewing.") | `src/app/NotificationSettings.tsx:245-252`; `preferences.ts:12` | DONE (ui/settings/notifications.rs, notifications.rs) | The preference, plus delivery for the DM being viewed (`notifications.rs` skips the channel being read). | S |
| Sound switch and per-category sound selects with preview | `src/app/NotificationSettings.tsx:20-75,89-148,253-283` | DONE (ui/settings/notifications.rs, sounds.rs) | Another agent is adding it. | — |
| Permission/status line ("This build can’t show system notifications.", "Notifications are off…", "Waiting for system permission…") | `src/app/NotificationSettings.tsx:149-160,199-216` | DONE (ui/settings/notifications.rs) | Report when the notification daemon is unavailable or refuses. | S |
| "Notification choices weren’t saved" toast with "Retry saving choices" / "Reload saved choices" | `src/app/NotificationSettings.tsx:291-311` | DONE (ui/settings/notifications.rs) | Error toast with retry and reload. | S |
| "Buzz couldn’t send the notification" error toast | `src/app/NotificationSettings.tsx:312-317` | DONE (ui/settings/notifications.rs) | Surface banner delivery errors. | S |
| No alert for the conversation in front of the user | `src/features/notifications/messages.ts:145-164`; `presentation.ts`; `src/features/messages/use-reading.ts:67-104` | DONE, deliberate difference (notifications.rs `in_front`, ui/workspace.rs `on_screen`) | buzz-app waits two frames and stays quiet only if the new row is in a reading lease's visible rows. The port stays quiet when the focused window shows the message's conversation: its channel for a top-level message, its open thread tab for a reply, even if the row is scrolled out of view. A reported visible row still counts. A reply whose thread is not open still notifies, as in buzz-app. An unfocused window always notifies; "Notify while viewing" notifies anyway. | S |
| Banners withdrawn once the message is read | None (banners stay in the notification center) | DONE (port addition, kept) (notifications/withdraw.rs, banners.rs, system.rs) | A message banner is closed when the focused window opens its conversation, or when an unread watch sees the message read (in view, marked read, or on another device). Linux uses `CloseNotification` with the id the service returned. macOS and Windows use GPUI's `dismiss_system_notification` by tag. The 64 most recent banners are remembered. A click or dismissal forgets the banner. Plugin notifications are not withdrawn. | S |

## 7. Shortcuts (ShortcutSettings.tsx, features/shortcuts)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Rows come from the live shortcut registry: Open Settings, Increase/Decrease/Reset interface size, Reload Buzz, Close tab or window (reserved on desktop), Home (Mod+Shift+A), Go back (Mod+[), Go forward (Mod+]), Search Buzz, Search this conversation, plus plugin groups (for example "Toggle channel terminal") | `src/app/ShortcutSettings.tsx:151-189`; `src/app/shortcuts.ts:29-150`; `src/app/shell/PageSearch.tsx:96-116` | DONE (crates/app/src/shortcuts.rs, crates/app/src/ui/settings/shortcuts.rs); Reload Buzz is left out as webview-only, and there are no plugin groups until plugins are ported | Build the rows from the actual keymap. Show "Reserved by Buzz" only for close/quit. | M |
| Rebind a shortcut: pencil button starts key capture (X cancels), shows the captured chord | `src/app/ShortcutSettings.tsx:396-432`; `src/features/shortcuts/KeyCaptureControl.tsx` | DONE (crates/app/src/ui/settings/shortcuts.rs) | Key-capture control and runtime rebinding of GPUI keymaps. | L |
| Capture validation: dead keys, Win/Cmd key refused, must include Ctrl or Alt, clipboard chords reserved, close/quit reserved, conflict "{chord} is already used by {title} ({owner})" | `src/app/ShortcutSettings.tsx:55-79,216-248` | DONE (crates/app/src/shortcuts.rs) | Validation rules and per-row error notices. | M |
| Row subtitles "Modified" and "Also used by …"; per-row "Reset" | `src/app/ShortcutSettings.tsx:194-201,379-395,433-445` | DONE (crates/app/src/ui/settings/shortcuts.rs) | Override tracking and display. | S |
| "Reset all shortcuts" button | `src/app/ShortcutSettings.tsx:303-315` | DONE (crates/app/src/ui/settings/shortcuts.rs) | Clear all overrides. | S |
| Persisted overrides, with "Retry saving shortcuts" on a save error | `src/app/ShortcutSettings.tsx:114-117,316-325`; `src/features/shortcuts/preferences.ts` | DONE (crates/app/src/shortcuts.rs, device pref `shortcut-bindings`) | Store overrides and surface save errors. | S |
| Warning after saving Enter or editor chords ("The message editor handles {chord} itself while you are typing…") | `src/app/ShortcutSettings.tsx:254-263` | DONE (crates/app/src/shortcuts.rs) | Post-save warning notice. | S |

## 8. Agents (AgentSettings.tsx, AgentDefaultsCard.tsx, ClaudeHarnessSetup.tsx, ArchiveSettings.tsx)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Hosts" block (add, reconnect or remove buzzd hosts; connection badges) | None | DONE (port addition: `ui/settings/agents.rs`) | Port-only addition for remote hosts. | S |
| "About ACP" help icon with the ACP hint tooltip | `src/app/AgentSettings.tsx:41-42,150-156` | DONE (`ui/settings/agents/harnesses.rs`) | Add the help IconButton and tooltip. | S |
| Harness status labels "Ready" / "CLI needed" / "Adapter needed" / "Unknown" | `src/app/AgentSettings.tsx:46-50,212-214` | DONE (`ui/settings/agents/harnesses.rs`; host status in `host/src/harness.rs`) | Map host `missing` to the reference labels. | S |
| Row set: Buzz Agent, Goose and Pi always listed (status "Unknown" if not reported); presets only when available; Claude Code in its own row | `src/app/AgentSettings.tsx:91-105,383-388` | DONE (`ui/settings/agents/harnesses.rs`) | Match the row policy. | S |
| Check failures: "Couldn’t check harnesses. Select Check again to retry." / "Couldn’t confirm harnesses. Showing the last check; try Check again." | `src/app/AgentSettings.tsx:172-196` | DONE (`ui/settings/agents/harnesses.rs`; `hosts.rs` tracks failed checks) | Error states from the host harness query. | S |
| Pi "Install" / "Update Pi" button, "Installing Pi and its ACP adapter…", result "…Restarted N waiting agents", error with the install log path and output | `src/app/AgentSettings.tsx:215-240,265-309` | DONE (`ui/settings/agents/harnesses.rs`; installer ported to `host/src/install.rs`) | Host install job (Node.js, Pi, adapter), restart waiting agents, report and log. | L |
| Pi manual setup: Node.js 22.19+, two npm commands with copy buttons, "{name} command copied." | `src/app/AgentSettings.tsx:43-45,58-61,128-140,310-377` | DONE (`ui/settings/agents/harnesses.rs`) | Collapsible manual-setup steps with copy buttons. | S |
| Preset harness (Hermes) not ready: "Uses the default model and credentials configured in {label}…" and "Manual {label} setup" with the setup-guide link | `src/app/AgentSettings.tsx:243-264,550-576` | DONE (`ui/settings/agents/harnesses.rs`). As in buzz-app, presets are listed only when installed, so the block shows only for a listed preset that is not ready. | Add the details section and the link. | S |
| Claude Code row: Install, "Checking sign-in…" / "Sign-in needed" / "Sign-in unconfirmed", install log, manual commands, "Sign in to Claude Code" login command | `src/app/ClaudeHarnessSetup.tsx:1-169` | DONE (`ui/settings/agents/harnesses.rs`; install, `ClaudeAuth` and login command in `host/src`) | Host API for the Claude auth check and install, plus the row UI. | M |
| "Add harness" button and dialog (preset catalog sidebar, Installed/Setup label, status, terminal setup hint, Executable, "Setup guide" link, Check again) | `src/app/AgentSettings.tsx:390-398,403-525` | DONE (`ui/settings/agents/harnesses.rs`) | The dialog. | M |
| Agent defaults: an inline, device-wide card on the page | `src/app/AgentSettings.tsx:526`; `src/app/AgentDefaultsCard.tsx:569-893` | DONE (`ui/agents/defaults.rs`). Decision: this computer's defaults are the inline card on Settings → Agents, as in buzz-app; each remote host has its own defaults, which open in a dialog from its row (Hosts block and Agents page). | Decide on placement. If kept per host, document it. | M |
| Warning "{KEY} overrides this provider selection. Replace or remove it under Environment variables." | `src/app/AgentDefaultsCard.tsx:206-240` | DONE (`ui/agents/defaults.rs`) | Provider override detection and warning. | S |
| Note "A saved GOOSE_PROVIDER override has a hidden value…" | `src/app/AgentDefaultsCard.tsx:673-679` | DONE (`ui/agents/defaults.rs`) | Add the note. | S |
| API key help: "{ENV} is used for model lookup and every local agent without its own value… Saved values are never shown again." | `src/app/AgentDefaultsCard.tsx:680-706` | DONE (`ui/agents/defaults.rs`) | Align it for the defaults form. | S |
| Browse models for Buzz Agent (Databricks "connect" with build host and filter) | `src/app/AgentDefaultsCard.tsx:388-460` | DONE (crates/app/src/ui/agents/defaults.rs, crates/app/src/hosts/models.rs) | - | M |
| "Search models" field when there are more than 10 models (up to 10 matches plus the selected one) | `src/app/AgentDefaultsCard.tsx:462-475` | DONE (`ui/agents/defaults.rs`): the defaults card now has buzz-app's search field and 10-match list. | Acceptable. Note the difference. | S |
| "Cancel model lookup" while a lookup runs | `src/app/AgentDefaultsCard.tsx:495-506` | DONE (`ui/agents/defaults.rs`). The host lookup itself runs on to its timeout; its result is dropped. | Cancellable lookup. | S |
| Lookup status lines: "Loading {label} models…", "Model choices loaded. Availability does not confirm inference access.", "No {label} models found…", "Cancelled. Retry when ready." | `src/app/AgentDefaultsCard.tsx:404,445-458,503` | DONE (`ui/agents/defaults.rs`) | Add the status lines. | S |
| Helper notes: "Install {label} to browse its models", "Choose a Goose provider to browse its models", "Choose a model for this Pi provider…", "Save environment removals before browsing models…" | `src/app/AgentDefaultsCard.tsx:528-558` | DONE (`ui/agents/defaults.rs`). "Model browsing requires an updated desktop app" does not apply: every host lists models. | Add the notes and the matching guards. | S |
| "Custom ID" option for Default effort | `src/app/AgentDefaultsCard.tsx:72-160,719-733` | DONE (`ui/agents/defaults.rs`, for provider, model and effort) | Custom effort entry. | S |
| Unset labels "Use build default ({x})" for Buzz Agent provider/model | `src/app/AgentDefaultsCard.tsx:44-48,196-205,317-326` | DONE (`ui/agents/defaults.rs`; build defaults sent by the host) | Show build defaults for buzz-agent. | S |
| After save: "Saved." / "Saved. Restarted N agents." / "…N agents couldn’t restart…" | `src/app/AgentDefaultsCard.tsx:620-633`; `src/features/agents/control.ts:162-169` | DONE (`ui/agents/defaults.rs`) | Show the restart summary. | S |
| Save disabled until something changes; Discard shown only when dirty | `src/app/AgentDefaultsCard.tsx:861-880` | DONE (`ui/agents/defaults.rs`) | Dirty tracking. | S |
| Environment variable row states "Set" / "Set on save" / "Removed on save" with Keep/Remove | `src/app/AgentDefaultsCard.tsx:765-798` | DONE (`ui/agents/defaults.rs`) | Align for the defaults form if strict parity is wanted. | S |
| "Saved agent activity · {community}" (Save agent activity, Save agent turn metrics, Activity retention with shorten confirmation, Clear activity history, Clear turn metrics, archive path and size, reload) | `src/app/ArchiveSettings.tsx:1-308` | DONE (`ui/settings/archive.rs`) | Another agent is adding it. | — |
| Native agent-activity archive (SQLite of encrypted 24200/44200 envelopes; settings, read, clear, ingest; budgets and retention) | `src-tauri/src/archive/mod.rs:1-196`; `src-tauri/src/archive/store.rs:1-332`; `src/features/archive/client.ts:1-110` | DONE (`crates/relay/src/archive.rs`, with settings, ingest, clear and retention) | Confirm the in-progress work includes the store, not just the UI. | — |
| "Messages → Remember mentioned agents" switch, plus "Agent preference wasn’t saved" toast | `src/app/AgentSettings.tsx:528-545` | DONE (`ui/settings/agents.rs`) | Another agent is adding it. | — |

## 9. User status (features/user-status)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Custom expiry date via a calendar popover (past days disabled) | `src/features/user-status/StatusExpiration.tsx:33-84` | DONE (`ui/status.rs` `date_menu`) | — | — |
| Status text capped at 100 characters in the input | `src/features/user-status/StatusEditor.tsx:186-194` | DONE (`ui/status.rs` `within_limit`) | — | — |
| Custom-emoji statuses (`:code:`) render the emoji image, with a text fallback | `src/features/user-status/StatusEmoji.tsx:1-52` | DONE (`ui/status.rs` `emoji`) | — | — |
| Other people's statuses: compact emoji with a tooltip on 1:1 DM rows, full status in the profile panel (`UserStatusDisplay`, `useUserStatus` watch) | `src/features/user-status/StatusDisplay.tsx:1-75`; `src/features/user-status/useUserStatus.ts:1-18` | DONE (`crates/relay/src/session/statuses.rs`, `ui/status.rs` `compact`/`display`; the profile card stands in for the profile panel) | — | — |

## 10. Identity names and profile targets (features/identity-names, features/profiles)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Namesake disambiguation: a public-key suffix ("Alice · pktz"), "{Owner}’s {Agent}" and "{Name} (agent)", with the viewer and the viewer's own agents given priority | `src/features/identity-names/policy.ts:11-112` | DONE (ui/identity.rs, with the messages rework) | A shared resolver used by messages, mentions and member lists. | M |
| Agent directory names: signed agent profile, then library alias, then same-community native agent name overriding the profile | `src/features/identity-names/directory.ts:15-149`; `src/features/identity-names/service.ts` | DONE (ui/identity.rs, with the messages rework) | Merge host agent names, scoped to the community. | M |
| App-local profile targets `buzz:agent-profile:<hex>` and `buzz:agent-instance:<json>` (agent avatar hint, instance profile panel) | `src/features/profiles/target.ts:1-36`; `src/features/profiles/instance-target.ts:1-52` | DONE (crates/app/src/ui/profile_panel.rs, profile_panel/agent.rs) | The instance target and panel | M |

## 11. Account actions (features/account-actions)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Plugin account actions in the profile menu, for example "Send feedback" (feedback dialog) | `src/features/account-actions/service.ts:1-41`; `src/app/shell/ProfileButton.tsx:421-430`; `src/bundled/feedback/index.tsx:1-14` | DONE (crates/app/src/ui/account.rs, crates/app/src/ui/feedback.rs) | - | - |
| A "Sign out" row in the account menu | Not in buzz-app (sign-out is only in Settings → Profile) | DONE (ui/account.rs: removed; sign-out is in Settings → Profile) | Remove it, or keep it as a deliberate addition. | S |

## 12. Plugins (Settings.tsx Plugins section, PluginImport.tsx) and plugin-dependent pieces

Bundled plugins are compiled in and switched on and off through buzz-app's plugin
manager (`crates/app/src/plugins.rs`); external plugins are skipped.

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Plugins section: one row per plugin with an enable switch; Channels shows "Required" | `src/app/Settings.tsx:219-364` (rows 277-315) | DONE (crates/app/src/ui/settings/plugins.rs, crates/app/src/plugins.rs) | - | L |
| Roll back / Reload / Delete per plugin, plus the rollback-blocked reason | `src/app/Settings.tsx:321-357` | SKIPPED (decided) | - | M |
| "External plugins are paused for this launch…", "Plugin settings couldn’t refresh", "Plugin change wasn’t confirmed", RecoveryScreen, "Plugin settings are unavailable…" | `src/app/Settings.tsx:244-275` | DONE for bundled plugins (crates/app/src/plugins.rs, crates/app/src/ui/settings/plugins.rs): the recovery screen, "Plugin change wasn’t confirmed" and "Plugin settings are unavailable…"; the paused and refresh notices concern external plugins and catalog polling, which this port does not have | - | S |
| "Load a plugin" → "Load from folder" | `src/app/PluginImport.tsx:120-146` | SKIPPED (decided) | - | M |
| "Load from Git": repository and branch/tag form, SSH/HTTPS help, Buzz git sign-in for community repositories, "Find plugins" | `src/app/PluginImport.tsx:147-195`; `src/app/Settings.tsx:235-242` | SKIPPED (decided) | - | M |
| Import preview: source and commit, candidate folders (radio), "Folders skipped (N)", declared host access with new/changed/removed grants, replace and rollback notice, Install/Update plugin, Close preview, discarding the token on unmount | `src/app/PluginImport.tsx:16-30,57-118,207-331` | SKIPPED (decided) | - | M |
| Plugin contributions shown in Settings: settings cards, notification categories, shortcut groups, account actions | `src/features/settings/service.ts:50-160`; `src/app/NotificationSettings.tsx:174-185`; `src/app/ShortcutSettings.tsx:168-183`; `src/features/account-actions/service.ts` | DONE (notification categories: crates/app/src/notifications/categories.rs; account action: crates/app/src/ui/account.rs; every contribution is hidden while its plugin is off). Settings cards come from the shared card registry (ui/settings/cards.rs, another slice), which should hide cards with `plugins::enabled_key`; shortcut groups belong to Terminal | - | L |
| Builderlab identity-binding signer (kind 24243, pinned origin) | `src-tauri/src/identity.rs:969-1032` | SKIPPED (decided) | - | S |
| Channel-recipe NIP-44 cipher (`kit_cipher`, used by Templates) | `src-tauri/src/identity.rs:1058-1083` | DONE (crates/relay/src/kit/mod.rs `seal`/`open`) | - | S |
| "Managed by" agent-owner preview (used by Templates) | `src/features/profiles/AgentOwnerPreview.tsx:1-70` | DONE (crates/app/src/ui/templates/owner.rs) | - | S |
| Sign-out also ends Builderlab and hosted-community logins | `src/app/ProfileSettings.tsx:404` | SKIPPED (decided) | - | S |

---

## buzz-app main of 2026-10-14

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Settings → Agents without v1: no harness list, Claude/Codex/Pi setup or agent defaults; Hosts block (this port's), saved agent activity and Remember mentioned agents remain (#859 `29c36a8b`) | src/app/AgentSettings.tsx | DONE (ui/settings/agents.rs) | Hosts copy now says agents keep their keys there and stay saved when a host is removed | S |
| Sign-out keeps the Agents list beside the old controller's list and defaults; removing agents deletes the old controller's keys when its list exists, then every Agents key, for both the agents and agents2 folders; erase note "Kept agents keep their names, types and keys; their settings on this device are erased." (#858, #859, #871) | src-tauri/src/{lib,sign_out}.rs; src/features/identity/SignOut.tsx | DONE (sign_out.rs, ui/settings/profile/sign_out.rs) | — | S |
| Pair mobile: "Copy pairing code" under the QR copies the code's text and reads "Copied" until a new code shows; its slot is kept in every phase (#1003) | src/bundled/pairing/PairingSettings.tsx; src/features/pairing/client.ts; src-tauri/src/pairing/mod.rs | DONE (app/src/ui/settings/pairing/mod.rs `copy_slot`; pair-mobile/src/lib.rs `Status::Qr::uri`) | DIFFERENT: no "Couldn’t copy. Try again" state, since GPUI's clipboard write reports no failure | S |
| Settings → Personal groups removed with personal groups (#988) | src/bundled/channels/index.tsx | DONE (app/src/ui/settings/personal_groups.rs deleted) | — | S |
| Native nsec backup HTTP client (#995) | crates/nsec-backup | N/A (library crate only; nothing in buzz-app's src/ or src-tauri uses it yet) | — | — |
| Settings → Messages, first under the community: "Open threads in" the side pane or the main pane, this device's default for new thread tabs (timeline, routes and links, another conversation's tab); open threads stay where placed; a failed save stays active with "Thread preference wasn’t saved" and Retry saving (`55b71bf1`) | src/bundled/channels/MessageSettings.tsx; src/features/messages/thread-preferences.ts | DONE (app/src/ui/settings/messages.rs, ui/settings/mod.rs, ui/workspace/board.rs, main.rs) | — | S |

## Not applicable: macOS/Windows only

- Dock unread badge settings ("Dock unread badge", Allow notifications and
  badges, Set up Dock badges, Check Dock permission, the "Dock badge
  unavailable" toast): `src/app/UnreadIndicatorSettings.tsx:1-71`,
  `src/app/NotificationSettings.tsx:287-290`.
- Notification permission buttons "Allow notifications" / "Check permission".
  These only render when the platform is not system-managed, which is macOS
  only: `src/app/NotificationSettings.tsx:217-240`,
  `src/features/notifications/platform.ts:42-43`.
- Keychain error codes (-25299, -128, -25293, -25308, -25300) and Keychain
  storage: `src-tauri/src/identity.rs:169-208`.
- Windows credential-store backend: `src-tauri/src/identity.rs:209-246`
  (Windows half of the cfg).
- The sign-out wipe of `~/Library/WebKit/<id>`: `src-tauri/src/sign_out.rs:104-105`.
- macOS title-bar drag handlers on the identity setup screen:
  `src/features/identity/IdentitySetup.tsx:22-34`.
- Apple-specific shortcut capture message ("Control isn’t used for
  shortcuts on this device. Try Command or Option."):
  `src/app/ShortcutSettings.tsx:223-227`.

## Not applicable: web or dev-server only

These were left out of the tables because they only exist in a browser or
localhost dev-server build.

- The Developer section (Broker activity, client metrics panel, Runtime
  settings → Log level, Caches → Clear cache) and its "Development" nav
  group: `src/app/DeveloperSettings.tsx:1-211`,
  `src/app/settings-sections.ts:31-35,50-54,118-120`,
  `src/features/developer/*`. The port has none of it. "Clear cache" may be
  worth a native equivalent.
- "Open the desktop app to load plugins from a folder or Git repository.":
  `src/app/Settings.tsx:224-228`.
- "Harness detection requires the desktop app." and "Model browsing
  requires an updated desktop app.": `src/app/AgentSettings.tsx:172-175`,
  `src/app/AgentDefaultsCard.tsx:534-538`.
- The development notification pause (`BUZZ_DEV_NOTIFICATIONS=0`):
  `src/app/NotificationSettings.tsx:149-151`.
- The broker archive location text ("Saved in SQLite on the development
  broker’s machine…"): `src/app/ArchiveSettings.tsx:136-138`.
- The wipe refusal in development builds and the `BUZZ_DEV_VIEWER` /
  `BUZZODZ_HOME` refusals: `src-tauri/src/sign_out.rs:716-751`.

---

## Counts

| Status | Count |
|---|---|
| MISSING | 0 |
| PARTIAL | 0 |
| DIFFERENT | 0 |
| DONE | 99 |
| SKIPPED (decided) | 6 |

Counts are of the rows in the tables. Nothing is open.
