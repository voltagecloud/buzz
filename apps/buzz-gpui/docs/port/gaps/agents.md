# Parity audit: agents slice (buzz-app → buzz-gpui)

Scope: buzz-app `src/features/{agents,agents2,host,sessions,pairing,workflows}`, `src/bundled/{agents,agents2,agent-activity,sessions,terminal,workflows,bestie,builderlab,pairing}`, `src/agent-plugins`, `src/plugins`, `src/buzz-mcp`, and the native agent, terminal, pairing and OAuth modules in `src-tauri/src`. The audit covers the main checkout only; `.claude/worktrees/` was ignored. Remote agent hosts (buzzd, Add host, per-host sections, host badges) are intended additions and are not listed as gaps.

## Path conventions

- **buzz-app paths** are relative to `/home/ben/projects/buzz-app`. Short forms:
  - `BA/` = `src/bundled/agents/`
  - `FA/` = `src/features/agents/`
  - `ST/` = `src-tauri/src/`
- **Port paths** are relative to `/home/ben/projects/buzz-gpui/crates`. Short forms:
  - `ui/agents.rs` = `app/src/ui/agents.rs`
  - `ui/agents/*.rs` = `app/src/ui/agents/*.rs`
  - `host/` = `host/src/`
- **Size:** S = hours, M = one to three days, L = a week or more.

## Work in progress elsewhere

Another agent is adding these. They are marked **IN PROGRESS** wherever they appear:

- agent restart on save;
- provider "Test connection";
- Databricks workspace and filter fields;
- Databricks "Disconnect";
- Escape closing dropdowns;
- clearing an agent's effort.

---

## 1. Agents page frame and inventory (`BA/AgentsPage.tsx`, `AgentControlPanel.tsx`, `UnifiedInventory.tsx`, `InventoryView.tsx`, `inventory-*.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Body when no desktop control or runtime unavailable: "Open the desktop app to import and run agents…", plus the teams and old-library slots | BA/AgentsPage.tsx:358-365, 466-473 | DIFFERENT (kept): the port always has an embedded host. With it offline the page still lists relay-only agents, archives and profiles from the relays (ui/agents.rs) | Nothing while every GUI ships a host | S |
| Legacy `ManagedAgents` body for older hosts (header "Individual agents" + Use-here explainer + "No agents yet. Create an agent or import one from old Buzz below.") | BA/AgentsPage.tsx:483-534 | DIFFERENT (kept): a host without the new snapshot fields still gets the full page; missing fields read as unavailable (host/api.rs) | Nothing | S |
| Old-library slot: "Connect to a community to browse the old library." plus "Retry connection" | BA/AgentsPage.tsx:241-252 | DIFFERENT (kept): buzz-app shows it only in the no-host body, which this port does not have (row above) | Only with a no-host body | S |
| Profile companion: clicking a tile opens the agent's profile PanelCard ("View profile for {name}") | BA/AgentsPage.tsx:163-197, 253-267; BA/AgentCard.tsx:394-405 | DONE (ui/agents/card.rs: the tile and "View profile" open the profile in the companion) | — | M |
| Routed edit target: a page route `{pubkey}` opens Edit for that agent, with opened, not-found and unavailable outcomes | BA/edit-route.ts:4-13; BA/index.tsx:23-27; BA/AgentControlPanel.tsx:146-178 | DONE (ui/agents.rs `edit_route`, `open_edit_route`): not-found and unavailable show a notice, as page routes here report no outcome to navigation | — | M |
| Display names resolved through the relay identity-names service, with disambiguation | BA/AgentControlPanel.tsx:116-131 | DONE (ui/agents/inventory.rs, ui/agents/discovery.rs) | — | S |
| "Waiting for the desktop app…" while a control operation is busy | BA/AgentControlPanel.tsx:323 | DONE (ui/agents.rs) | — | S |
| Controller `state.error` shown as alert or status | BA/AgentControlPanel.tsx:310-314 | DONE (ui/agents.rs, app/src/hosts/control.rs) | — | S |
| Inventory groups: "Individual agents" (local in this community, tiles); "Local agents in other communities"; "Available to import"; "Relay-only agents" (rows); community subsections; "Community unknown" | BA/InventoryView.tsx:169-243; BA/inventory-decisions.ts:4-7 | DONE (ui/agents/inventory.rs, ui/agents.rs) | — | L |
| Row layout (`.agent-inventory-row`) with chevron details toggle, and a one-column layout at ≤640px | BA/AgentControls.css:16-91; BA/InventoryIdentityCard.tsx:350-385 | DONE (ui/agents/card.rs) | — | M |
| "Archived (N)" accordion and "All your agents are archived in this community." | BA/InventoryView.tsx:267-300 | DONE (ui/agents.rs) | — | M |
| "Profiles without identities" section | BA/InventoryView.tsx:302-316 | DONE (ui/agents.rs, ui/agents/discovery.rs) | — | M |
| Teams slot on the Agents page (channel-templates) | BA/AgentsPage.tsx:345-350; BA/InventoryView.tsx:270 | DONE (crates/app/src/ui/agents/teams.rs) | - | M |
| Community inventory status lines: "Checking community inventory…"; "Agent names and pictures could not be checked for {community}. Refresh to retry."; "Community inventory could not be checked…"; "Showing known community associations. Community checks are not current." | BA/UnifiedInventory.tsx:384-404; BA/use-community-inventory.ts | DONE (ui/agents.rs, ui/agents/discovery.rs) | — | M |
| Inventory warnings: "{warning} Retry local discovery by reopening the app." | BA/UnifiedInventory.tsx:342-346 | DONE (host/lib.rs, ui/agents.rs) | — | S |
| "Refresh agents" also refreshes the relay library, archives and community reads | BA/UnifiedInventory.tsx:356-371 | DONE (ui/agents.rs, ui/agents/discovery.rs) | — | S |
| Background fetch of public profiles (names, pictures) for agent keys | BA/UnifiedInventory.tsx:205-214 | DONE (ui/agents/discovery.rs, ui/agents/relay.rs) | — | S |
| Focus and scroll to a newly imported card; focus returns to "Actions for…" after archive | BA/InventoryView.tsx:150-167; BA/AgentCard.tsx:107-112 | DONE (ui/agents/card.rs, ui/agents.rs `FocusTarget`): an imported or created card scrolls into view and focuses "Review agent status", else its menu, once no dialog is open; a confirmed Archive focuses the "Archived (N)" header and Unarchive the card's menu | — | S |

## 2. Agent card (`BA/AgentCard.tsx`, `FA/AgentAvatar.tsx`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Presence dot driven by relay presence (online, away, offline, unknown), re-read every 5s up to six times after start or stop | BA/AgentCard.tsx:113-141, 186 | DONE (ui/agents.rs `recheck_presence`; relay `Session::refresh_presence`) | — | M |
| Thinking badge on the squircle avatar | BA/AgentCard.tsx:176-184 | DONE (ui/agents/card.rs) | See §17-H | M |
| Model line: `launchModel`; "Custom model" when set through the environment; `harness.model`; "Default model". The harness is not shown | BA/AgentCard.tsx:153-160, 200-212 | DONE (ui/agents.rs) | — | S |
| Tile has no process status or buttons; process control lives in Manage | BA/AgentCard.tsx:386-420 | DONE (ui/agents/card.rs) | — | S |
| "Review agent status" button when just imported, failed, profile pending, or there is a notice | BA/AgentCard.tsx:409-418; BA/AgentsPage.tsx:500-506 | DONE (ui/agents/card.rs) | — | S |
| "Archived" pill | BA/AgentCard.tsx (archived label) | DONE (ui/agents/card.rs) | — | S |
| Menu item "View profile" | BA/AgentCard.tsx:240-252 | DONE (ui/agents/card.rs) | — | S |
| Menu items "Archive agent" / "Unarchive agent" (icons; disabled while pending) | BA/AgentCard.tsx:270-287 | DONE (ui/agents/card.rs, ui/agents/dialogs.rs) | — | S |
| Menu item "Duplicate", which opens Add agent as "Duplicate {name}" with name "{name} copy" | BA/AgentCard.tsx:322-340; BA/AgentControlPanel.tsx:105-110; BA/AgentCreateDialog.tsx:186-193, 338 | DONE (ui/agents/card.rs, ui/agents/create.rs) | — | S |
| Menu item "Share" (direct share or export) | BA/AgentCard.tsx:342-350 | DONE (ui/agents/card.rs, ui/agents/share.rs) | — | S |
| Several local setups: "Edit/Duplicate/Delete {name}" choice rows with relay URL and npub | BA/AgentCard.tsx:296-365 | DONE (ui/agents/card.rs) | — | S |
| Disabled Edit with a note: "Import this identity to edit in Foundation." / "No linked identity to edit." | BA/AgentCard.tsx:368-380 | DONE (ui/agents/card.rs) | — | S |
| Identity popover "{name} identity details" with npubs | BA/AgentCard.tsx:448-474 | DONE (ui/agents/card.rs `card_menu`): a row with no setup, profile or archive opens the popover from its "Actions for {name}" button; buzz-app's rows here have no menu at all | — | S |
| "No linked identity" | BA/AgentCard.tsx:476-480 | DONE (ui/agents/card.rs) | — | S |
| Feedback slot: "Archiving…" / "Unarchiving…" / "{Archive\|Unarchive} failed: {error}" with Retry | BA/InventoryIdentityCard.tsx:200-235 | DONE (ui/agents/card.rs) | — | S |
| Port-only menu items "Start on launch" (check) and "Retry profile publication" | n/a (buzz-app has no start-on-launch UI; profile retry is in Manage and Edit) | DIFFERENT (kept): "Start on launch" stays because the host restores those agents; "Retry profile publication" moved to Manage and Edit (ui/agents/card.rs) | Decide whether to keep Start on launch | S |
| Picture shown through `useAvatarPreview` (relay-hosted media preview) | BA/AgentCard.tsx:142-152 | DONE (ui/agents/card.rs `picture` through `media::avatar`, the authenticated loader) | — | S |

## 3. Manage dialog and process control (`BA/ManagedAgentActions.tsx`, `BA/InventoryIdentityCard.tsx`, `FA/control.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Imported, not started. Start it when you are ready." / "…Choose Use here to set up this identity in a community." | BA/ManagedAgentActions.tsx:50-58 | DONE (ui/agents/card.rs) | — | S |
| "Checking agent status…" and outcome notices: "The agent didn't {start}. {reason} Try again." / "We couldn't confirm whether the agent started. Refresh status before trying again." | BA/ManagedAgentActions.tsx:88, 140-231 | DONE (ui/agents/card.rs) | — | S |
| Launch-block reasons: "Choose Use here before starting this imported identity.", "Refresh status before starting.", "Waiting for the current operation.", "The bundled agent runtime is unavailable.", "Waiting to start; unlock Keychain if prompted.", "Waiting for the process transition." | FA/control.ts:379-397 | DONE (app/src/hosts/control.rs) | — | S |
| Stop as recovery: allowed after an error or with an enabled pending launch | FA/control.ts:406-422 | DONE (app/src/hosts/control.rs) | — | S |
| One operation at a time: "Another agent operation is in progress." / "Refresh local agents before trying again." | FA/control.ts:510-531 | DONE (app/src/hosts/control.rs) | — | S |
| Standard failure copy: "…Could not confirm the operation. Check current status and saved settings before retrying… Your edits are retained." | FA/control.ts:538-548 | DONE (app/src/hosts/control.rs) | — | S |
| Restart lives in Edit → Runtime, not in Manage | BA/ManagedAgentActions.tsx:100-135 | DONE (ui/agents/edit.rs) | — | S |
| Identity actions in Manage: installation select ("Choose an installation" / Installed / Development Buzz), Import, Use here, Clone (with tooltip), Remove | BA/InventoryIdentityCard.tsx:100-347 | DONE (ui/agents/card.rs, ui/agents/dialogs.rs) | — | L |
| "Identity & sources" details: communities, installations, npub | BA/InventoryIdentityCard.tsx:350-385 | DONE (ui/agents/card.rs) | — | S |
| Host diagnostics shown in Manage | BA/AgentEditor.tsx:302-309 (buzz-app shows them only in Edit → Technical details) | DONE (ui/agents/edit.rs) | — | S |
| Credential wait: dialogs can close and Stop stays available while a Keychain prompt is pending | FA/control.ts:313-319, 549-564 | DONE (app/src/hosts/control.rs, host/start.rs) | — | S |

## 4. Add agent dialog (`BA/AgentCreateDialog.tsx`, `AgentSettingsFields.tsx`, `AgentHarnessEditor.tsx`, `AgentEnvironmentEditor.tsx`, `ProviderApiKeyField.tsx`, `agent-edit.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Destination is the current community; there is no picker | BA/AgentCreateDialog.tsx:116-135; BA/AgentControlPanel.tsx:300-312 | DONE (ui/agents/create.rs; the Host select is the intended remote-host addition) | — | S |
| Sidebar "Import" button opens "Import agent snapshot" | BA/AgentCreateDialog.tsx:368-379 | DONE (ui/agents/create.rs, ui/agents/snapshots.rs) | — | M |
| Catalog "AGENTS" list in the sidebar, preview pane, "Add agent" / "Added to My Agents", "Adding agent…", "Withdrawn agent" | BA/AgentCreateDialog.tsx:380-465 | DONE (ui/agents/catalog.rs, ui/agents/create.rs) | — | M |
| Seeded drafts from catalog and clone (runtime, model, provider, picture, session policy) and the clone notice | BA/AgentCreateDialog.tsx:67-104, 340-360 | DONE (ui/agents/create.rs `seeded`, `seeded_draft`) | — | S |
| Default harness: the agent-defaults harness if available, else `buzz-agent` | BA/AgentCreateDialog.tsx:40-63 | DONE (ui/agents/draft.rs) | — | S |
| Default provider for Buzz Agent: the first provider (`databricks_v2`) unless defaults supply one | BA/AgentCreateDialog.tsx:56-62 | DONE (ui/agents/draft.rs) | — | S |
| Harness help: "Pi needs its CLI, Node.js and buzz-pi-acp…"; "{preset} needs its ACP launcher. Set it up under Settings → Agents → Harnesses."; link "Open Harnesses in Settings"; "Opening Settings discards unsaved edits." | BA/AgentHarnessEditor.tsx:103-131 | DONE (ui/agents/fields.rs) | — | S |
| Harness option "{label} (current executable)" for a saved preset command; label "Custom executable / current value" | BA/AgentHarnessEditor.tsx:60-77 | DONE (ui/agents/fields.rs) | — | S |
| A harness change resets args to the option's `defaultArgs`, the provider to its first provider, and the model to "" | BA/AgentHarnessEditor.tsx:78-101 | DONE (ui/agents/fields.rs) | — | S |
| Preset alert: "This agent has model or provider settings that Buzz cannot apply to {label} yet…", "Current model/provider", "Use {label} defaults" button; validation "Use {preset} defaults before saving this agent." | BA/AgentSettingsFields.tsx:280-310; BA/agent-edit.ts:81-82 | DONE (ui/agents/fields.rs, ui/agents/draft.rs) | — | S |
| `PresetSetupHint`: backticked commands as inline code; setup URL | FA/PresetSetupHint.tsx:1-8 | DONE (ui/agents/fields.rs) | — | S |
| Windows notices: Git Bash needed; "Databricks sign-in is not supported on Windows yet." | BA/AgentSettingsFields.tsx:224-236 | N/A (Windows builds are not shipped) | — | S |
| Hidden `GOOSE_PROVIDER` override notice | BA/AgentSettingsFields.tsx:237-243 | DONE (ui/agents/fields.rs) | — | S |
| API key field: eye toggle ("Show API key" / "Hide API key"); "Will remove on save"; helper "…Keys exported in your shell profile are not used. Saved keys are stored in this device's local agent settings files." plus Goose and Pi variants | BA/ProviderApiKeyField.tsx:32-70; BA/AgentSettingsFields.tsx:245-275 | DONE (ui/agents/fields.rs) | — | S |
| A typed key belongs to the provider it was typed for; changing the provider drops it | BA/AgentSettingsFields.tsx:171-187 | DONE (ui/agents/draft.rs) | — | S |
| Per-agent "Effort" select | not in buzz-app's agent form (effort exists only in defaults and snapshots: src/app/AgentDefaultsCard.tsx:720) | DONE (ui/agents/fields.rs; the per-agent Effort select was removed, as in buzz-app) | — | S |
| Clearing an agent's effort | — | DONE (host/api.rs `AgentEdit::effort`: `Some(None)` clears it) | — | S |
| Escape closes open selects and the model dropdown | Base UI Select / Combobox; BA/AgentModelPicker.tsx:388, 418 | DONE (ui/agents/create.rs, ui/agents/edit.rs, ui/agents/defaults.rs) | — | S |
| Owner-only build note: "This build allows instructions only from the owner and verified same-owner agents." | ui.md §5a.7 (BA/AgentSettingsFields.tsx) | DONE (ui/agents/fields.rs) | — | S |
| Footer helper variants for Pi and presets ("Configure {preset}'s model and sign-in in the harness itself…") | BA/AgentSettingsFields.tsx:404-412 | DONE (ui/agents/fields.rs) | — | S |
| Create pipeline: create, start, then "Finishing…" profile publication. Failures go to "Checking…" (refresh and classify) with: "We couldn't confirm whether the agent was created…"; "…was saved, but couldn't start… select Start agent"; "…started, but profile setup didn't finish… Select Finish profile"; "Finish profile" button | BA/AgentCreateDialog.tsx:180-312, 474-583 | DONE (ui/agents/create.rs) | — | M |
| Status lines: "{name} was saved. Finishing its profile…", "{name} was saved. Start it to finish setup.", "Connect to a community and use a rebuilt desktop app to create an agent.", runtime-unavailable alert | BA/AgentCreateDialog.tsx:525-545 | DONE (ui/agents/create.rs) | — | S |
| Extra footer "Retry status" when control is in error | BA/AgentCreateDialog.tsx:584-590 | DONE (ui/agents/create.rs) | — | S |
| Backdrop and Escape close only when not dirty and not busy | BA/AgentCreateDialog.tsx:325-330 | DONE (ui/agents/create.rs) | — | S |

## 5. Model picker (`BA/AgentModelPicker.tsx`, `FA/models.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Buzz Agent / Databricks catalog: "Browse models" signs in through the browser ("Loading models… sign in through your browser if asked.") | BA/AgentModelPicker.tsx:186-215; ST/agent_models.rs:500-630 | DONE (ui/agents/fields.rs, app/src/hosts/models.rs, app/src/hosts/sign_in.rs, host/models.rs) | — | L |
| Test connection button and result ("Sending a short test message…", "Connected. The model replied.", "Connected using {model}.", failure icon) | BA/AgentModelPicker.tsx:140-160, 332-353 | DONE (ui/agents/fields.rs, host/models.rs) | — | M |
| Advanced → Model accordion: "Model ID (custom or blank)" field | BA/AgentModelPicker.tsx:575-600 | DONE (ui/agents/fields.rs) | — | S |
| "Databricks workspace (HTTPS origin)" and "Model filter (optional)" with inherited placeholders; "Editing either field saves both displayed values." | BA/AgentModelPicker.tsx:612-646 | DONE (ui/agents/fields.rs) | — | M |
| "Disconnect" and "Credentials are shared within Foundation for this workspace… Disconnect removes this app's cache…" | BA/AgentModelPicker.tsx:647-668 | DONE (ui/agents/fields.rs) | — | S |
| "Refresh models" (Pi, Databricks) | BA/AgentModelPicker.tsx:602-610, 649-654 | DONE (ui/agents/fields.rs) | — | S |
| "Cancel model lookup" / "Cancel sign-in", then "Cancelled. Retry when ready."; one lookup at a time | BA/AgentModelPicker.tsx:509-521; FA/models.ts:30-120 | DONE (ui/agents/fields.rs, app/src/hosts/models.rs) | — | S |
| Empty-catalog statuses: Goose "No models found for this Goose provider…"; Pi "No signed-in Pi providers found…"; Databricks "No models found…"; Pi "No Pi models for this provider…" | BA/AgentModelPicker.tsx:230-240, 281, 543-545 | DONE (ui/agents/fields.rs) | — | S |
| Warnings: model not in Goose's list; not in Pi's catalog; "Choose a model for this provider before starting, or clear Provider…"; `GOOSE_MODEL` / `BUZZ_AGENT_MODEL` override wording; Goose info paragraph | BA/AgentModelPicker.tsx:525-572 | DONE (ui/agents/fields.rs) | — | S |
| Goose list cap note "Showing up to N models. Type to search all {M}." | BA/AgentModelPicker.tsx:488-493 | DONE (ui/agents/fields.rs) | — | S |
| Pi catalog loads as soon as Pi is selected; models are filtered to the chosen provider | BA/AgentModelPicker.tsx:150-200 | DONE (ui/agents/fields.rs) | — | S |
| Combobox: Enter commits the highlighted item; typed text marks the dialog dirty; items show "{id}" and " · Custom ID" | BA/AgentModelPicker.tsx:355-480 | DONE (ui/agents/fields.rs) | — | S |

## 6. Edit agent dialog (`BA/AgentEditor.tsx`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Avatar editor (upload and change picture, squircle) with picture validation | BA/AgentEditor.tsx:127-131, 185-200 | DONE (ui/agents/edit.rs) | — | M |
| Runtime accordion buttons: Start / "Retry start", Stop, Restart / "Restart to apply" | BA/AgentEditor.tsx:243-270 | DONE (ui/agents/edit.rs) | — | S |
| Runtime mode line variants: "Imported · close the editor and choose Use here before starting", "Enabled intent saved · execution unavailable", "Enabled · starts with buzz-app", "Enabled · manual-start only", "Start on launch enabled", "Manual start · a later sent mention can start this agent" | BA/AgentEditor.tsx:229-242 | DONE (ui/agents/edit.rs) | — | S |
| Revision note: "Running revision none", "Saved changes are not running yet. Stop ends current work…" | BA/AgentEditor.tsx:271-277 | DONE (ui/agents/edit.rs) | — | S |
| Runtime and Technical details as collapsed accordions | BA/AgentEditor.tsx:216-312 | DONE (ui/agents/edit.rs) | — | S |
| "Public key (npub)", npub-encoded and select-all | BA/AgentEditor.tsx:287-294 | DONE (ui/agents/edit.rs) | — | S |
| Save restarts running agents whose effective settings changed; toast "Saved." / "Saved. Restarted N agents." / restart-failure notice kept in the dialog | BA/AgentEditor.tsx:160-171; FA/control.ts:162-169; ST/agents.rs:970-1035 | DONE (host/start.rs, ui/agents/edit.rs) | — | M |
| After save, profile publication with "Settings saved. Publishing profile update…" / "Settings saved; profile publication is unconfirmed…" | BA/AgentEditor.tsx:148-160 | DONE (ui/agents/edit.rs) | — | S |
| "Retry profile publication" button and "Profile published. Running work was not restarted." | BA/AgentEditor.tsx:320-345 | DONE (ui/agents/edit.rs) | — | S |
| `agent.error` and `state.error` alerts; "Retry status" | BA/AgentEditor.tsx:308-318, 347-351 | DONE (ui/agents/edit.rs) | — | S |
| Stale copy: "The host has a newer saved revision. Your edits are still here; copy anything you need, then discard to load the latest settings." | BA/AgentEditor.tsx:352-358 | DONE (ui/agents/edit.rs) | — | S |
| Close blocked while dirty or uploading | BA/AgentEditor.tsx:98-101 | DONE (ui/agents/edit.rs) | — | S |

## 7. Delete dialog (`BA/AgentDeleteDialog.tsx`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Retry status" button inside the dialog when control is in error; fallback "Could not confirm deletion." | BA/AgentDeleteDialog.tsx:30-36, 70-75 | DONE (ui/agents/dialogs.rs) | — | S |

## 8. Import from old Buzz (`BA/AgentImport.tsx` vs `ui/agents/import.rs`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Entry point: an "Import from another installation" accordion on the Agents page, or a modal opened from an "Available to import" row | BA/AgentImport.tsx:281-287; BA/AgentControlPanel.tsx:375-431 | DONE (ui/agents/import.rs, ui/agents/card.rs) | — | S |
| Intro copy (copies key and settings, leaves the agent stopped, explains repair, says to stop the old agent and turn off its auto-start) | BA/AgentImport.tsx:288-295 | DONE (ui/agents/import.rs) | — | S |
| Browsing without a destination ("browsing does not need a connection") | BA/AgentImport.tsx:299-304 | DONE (ui/agents/import.rs) | — | M |
| "Community: {destination}" line | BA/AgentImport.tsx:296-298 | DONE (ui/agents/import.rs) | — | S |
| "Import options" section, open by default when there is no destination | BA/AgentImport.tsx:400-459 | DONE (ui/agents/import.rs) | — | S |
| Destination community text field | BA/AgentImport.tsx:432-443 | DONE (ui/agents/import.rs) | — | S |
| "Load agents" button | BA/AgentImport.tsx:444-449 | DONE (ui/agents/import.rs) | — | S |
| Source path and warnings, deduplicated | BA/AgentImport.tsx:451-458 | DONE (ui/agents/import.rs) | — | S |
| Loading copy "Loading agents from old Buzz…" | BA/AgentImport.tsx:314 | DONE (ui/agents/import.rs) | — | S |
| Load error "Could not load agents from old Buzz. Check Import options and try again." | BA/AgentImport.tsx:75-81, 315-325 | DONE (ui/agents/import.rs) | — | S |
| Refresh agent status before each preview | BA/AgentImport.tsx:69-72 | DONE (ui/agents/import.rs) | — | S |
| Candidates whose key is already managed are hidden | BA/AgentImport.tsx:94-105 | DONE (ui/agents/import.rs) | — | S |
| Empty-state variants ("No agents left to import or repair…", "No agents in this local library.", single-agent unavailable) | BA/AgentImport.tsx:326-334 | DONE (ui/agents/import.rs) | — | S |
| Per-candidate status "Not imported" / "Team instructions not imported" | BA/AgentImport.tsx:346-348 | DONE (ui/agents/import.rs) | — | S |
| Pubkey under a collapsed "Identity" disclosure | BA/AgentImport.tsx:354-359 | DONE (ui/agents/import.rs) | — | S |
| Per-row Import button ("Import {name}") | BA/AgentImport.tsx:383-396 | DONE (ui/agents/import.rs) | — | S |
| "Repair team import" for `needsTeamImport` agents, with its status copy; page accordion "Repair team import from another installation" | BA/AgentImport.tsx:101-118, 308-313, 336-338, 392-395; BA/AgentControlPanel.tsx:375-394 | DONE (ui/agents/import.rs) | — | M |
| "Clone to this community" (create form seeded from the record, new key); "Could not read clone settings…" | BA/AgentImport.tsx:361-382 | DONE (ui/agents/import.rs) | — | M |
| After import, close and hand the imported agent to the page (focus the card) | BA/AgentImport.tsx:120 | DONE (ui/agents/import.rs) | — | S |
| Failed commit: "Import didn't finish. Reload the source before trying again." | BA/AgentImport.tsx:121-130 | DONE (ui/agents/import.rs) | — | S |
| "Import is unavailable in this app session." | BA/AgentImport.tsx:223-225, 305-307 | DONE (ui/agents/import.rs) | — | S |
| Single-agent mode: "Import {name}?" dialog with Source/Destination, explanation, "Importing…" | BA/AgentImport.tsx:135-280 | DONE (ui/agents/import.rs) | — | M |
| Single-agent: "Use destination" (personal space); "Choose another source"; "Reload source"; "Loading agent…" | BA/AgentImport.tsx:149-160, 177-255 | DONE (ui/agents/import.rs) | — | S |

## 9. Use here and Clone (`BA/LocalInventoryAction.tsx`, `BA/AgentControlPanel.tsx`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Set up agent here" dialog: "Set up the imported agent in this community. It will not start yet.", "Destination: …", "Use here" (resolves the community, finishes the import) | BA/AgentControlPanel.tsx:432-481; BA/LocalInventoryAction.tsx:40-80; ST/agents.rs:1354-1365 | DONE (ui/agents/dialogs.rs, host/lib.rs) | — | M |
| "Review agent to clone" dialog with "Review clone", which opens Add agent "Clone agent" with the clone notice | BA/AgentControlPanel.tsx:443-449; BA/LocalInventoryAction.tsx:52-53, 81-93; ST/agents.rs:1366-1386 | DONE (ui/agents/dialogs.rs, ui/agents/create.rs) | — | M |
| Hints and errors: "Connect to the destination community to use this identity there.", "Use here is unavailable.", "Cloning this agent is unavailable in this app version.", "Could not complete this action. Refresh and try again.", "This app cannot set up this agent yet. Import it first…" | BA/LocalInventoryAction.tsx:55-101; BA/AgentControlPanel.tsx:472-476 | DONE (ui/agents/dialogs.rs) | — | S |

## 10. Agent snapshots: export, import, chat preview, direct share (`BA/AgentSnapshots.tsx`, `BA/SnapshotPreview.tsx`, `BA/DirectShare.tsx`, `FA/snapshot*.ts`, `FA/SnapshotShareDialog.tsx`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Export {name}" dialog (Cancel / Export; cannot close while exporting) | BA/AgentSnapshots.tsx:216-243 | DONE (ui/agents/snapshots.rs; card menu "Share" falls back to it without a community) | Dialog and menu entry | M |
| Memories select: Agent only / + core memory / + all memories | BA/AgentSnapshots.tsx:245-265 | DONE (ui/agents/snapshots.rs, snapshot/memory.rs) | Memory read | M |
| File format select: PNG / JSON | BA/AgentSnapshots.tsx:266-279 | DONE (snapshot/png.rs, ui/agents/snapshots.rs) | PNG tEXt encoder | M |
| Disclosure about portable config and secrets; "Review portable configuration" JSON preview; artwork source | BA/AgentSnapshots.tsx:280-295 | DONE (ui/agents/snapshots.rs) | — | S |
| Required "I reviewed the portable configuration…" checkbox, reset when the agent changes | BA/AgentSnapshots.tsx:98-105, 296-307 | DONE (ui/agents/snapshots.rs) | — | S |
| Memory plaintext warning and confirmation checkbox | BA/AgentSnapshots.tsx:308-336 | DONE (ui/agents/snapshots.rs) | — | S |
| Eligibility rules (buzz-agent only, owner-only, no env or backend) and credential-looking text blocked | FA/snapshot.ts:160-164, 392-452 | DONE (snapshot/manifest.rs) | — | S |
| PNG embeds the avatar as 512px artwork, else a placeholder; downloads `{name}.agent.{png,json}`; "Agent configuration changed. Review it again before exporting." | BA/AgentSnapshots.tsx:24-33, 163-207 | DONE (snapshot/png.rs, ui/agents/snapshots.rs; saves through the native save dialog) | Image resize, save dialog | M |
| "Import agent snapshot" dialog with "Choose file" (.json, .png, legacy) | BA/AgentSnapshots.tsx:556-662 | DONE (ui/agents/snapshots.rs; opened by the Add agent sidebar "Import") | File picker and parser | M |
| Legacy-file error (.persona.*, .zip); size limits; parse errors | FA/snapshot.ts:7-25, 187-218, 759-845 | DONE (snapshot/manifest.rs, ui/agents/snapshots.rs) | — | S |
| Preview: name, instructions, "No system prompt included.", fresh-keypair note, runtime/response/parallelism/session summary, "Full embedded manifest" | BA/AgentSnapshots.tsx:663-742 | DONE (ui/agents/snapshots.rs) | — | S |
| Memory restore option ("Restore memory under the new agent identity"), unavailable fallback, "No memory included — config only." | BA/AgentSnapshots.tsx:680-714; ST/agents.rs:1595-1612 | DONE (ui/agents/snapshots.rs, host/snapshot.rs) | Host memory write | M |
| "Import is blocked: …" list; runtime mapped to an installed harness ("{runtime} is unavailable. Install it in Settings first.") | BA/AgentSnapshots.tsx:715-720; FA/snapshot.ts:502-603 | DONE (snapshot/manifest.rs) | — | S |
| Upload embedded artwork; "Creating agent…"; duplicate-identity warning on failure | BA/AgentSnapshots.tsx:507-551, 745 | DONE (ui/agents/snapshots.rs) | — | S |
| "Agent imported" result with "Publish profile"; partial memory error and "Retry memory restore" | BA/AgentSnapshots.tsx:438-488, 536-539, 567-636 | DONE (ui/agents/snapshots.rs) | — | S |
| Chat attachment card for snapshot files with "Add agent" / "Add team" | src/features/messages/SnapshotAttachment.tsx:11-50; FA/snapshot-preview.ts:15-29 | DONE (ui/agents/snapshot_preview.rs; "Add team" opens through a hook teams register) | — | M |
| Link context menu "Preview snapshot"; "Preview snapshot" dialog (loading, error + Retry, Cancel); authenticated read with SHA-256 check; agent vs team detection | src/features/conversation/MessageLink.tsx:276-289; BA/SnapshotPreview.tsx:116-197; FA/snapshot-preview.ts:66-159 | DONE (ui/agents/snapshot_preview.rs, ui/links.rs) | — | M |
| Pasting a copied snapshot link attaches the file | FA/snapshot-clipboard.ts:17-67 | DONE (ui/agents/snapshot_preview.rs, ui/composer.rs; within this app: GPUI on Linux has no text/html clipboard, so the payload travels as clipboard metadata) | — | M |
| "Share {name}" dialog: review block, recipient picker, Send / "Sending…" / "Retry send", toast "Sent a copy of {name}" | FA/SnapshotShareDialog.tsx:192-383; BA/DirectShare.tsx:69-218; FA/snapshot-send.ts:24-103 | DONE (ui/agents/share.rs, snapshot/send.rs) | — | M |
| Pending sends survive restart ("Retry the earlier message to confirm its delivery before editing.") | FA/SnapshotShareDialog.tsx:121-162, 385-389; FA/snapshot-recovery.ts | DONE (ui/agents/share.rs, snapshot/share.rs; journaled in the session outbox) | — | M |
| "What's included" select and plaintext warning; "Share memories?" confirmation | FA/SnapshotShareDialog.tsx:390-427, 457-478 | DONE (ui/agents/share.rs) | — | S |
| "Copy link" (upload, copy URL + HTML; "Copying…" / "Copied") | FA/SnapshotShareDialog.tsx:428-439; FA/snapshot-link.ts | DONE (ui/agents/share.rs; plain text is the URL, the named link rides as clipboard metadata, see the paste row) | Rich clipboard | M |
| Export switch inside Share; "Share to catalog" switch; error copy | FA/SnapshotShareDialog.tsx:145-307, 348-350; BA/DirectShare.tsx:164-227 | DONE (ui/agents/share.rs, ui/agents/catalog.rs, ui/agents/team_dialogs.rs) | — | S |

## 11. Community catalog (`BA/CommunityCatalog.tsx`, `FA/catalog*.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Catalog reader (kinds 30175/30178, paged, newest version per entry, 5,000-event cap) | FA/catalog.ts:70-304 | DONE (relay/src/catalog.rs, relay/src/session/catalog.rs) | Relay-crate reader | L |
| "Share to catalog" switch with agent and team copy | BA/CommunityCatalog.tsx:46-49, 79-163 | DONE (ui/agents/catalog.rs) | — | M |
| Share notices (queued, published, no longer discoverable, stalled + Retry / Dismiss); "Failed to update catalog sharing." | BA/CommunityCatalog.tsx:61-72, 164-236 | DONE (ui/agents/catalog.rs) | — | M |
| Publish and unshare rules; agent and team validation (limits, invisible characters, unsupported runtime, allowlist downgraded) | FA/catalog.ts:387-446; FA/catalog-protocol.ts:152-300 | DONE (relay/src/catalog.rs) | — | M |
| "Community Catalog" browser dialog (Agents/Teams sidebar, loading/error/"Nothing shared yet") | BA/CommunityCatalog.tsx:495-643 | DONE (ui/agents/catalog.rs) | — | M |
| Agent detail (avatar, "Added by", metadata grid, instructions); team detail (members, expandable rows) | BA/CommunityCatalog.tsx:652-711, 811-892 | DONE (ui/agents/catalog.rs) | — | S |
| "Add agent" pre-fills the create form; "Added to My Agents" remembered per community; "Add team" / "Added to my teams" | BA/CommunityCatalog.tsx:338-427, 555-590; FA/catalog.ts:334-357 | DONE (ui/agents/catalog.rs, ui/agents/create.rs, ui/agents/teams.rs) | — | M |
| Non-stock ACP transport blocked ("…uses the X transport, which this app can't run yet…") | FA/catalog-protocol.ts:111-123; BA/CommunityCatalog.tsx:713-722 | DONE (relay/src/catalog.rs, ui/agents/catalog.rs) | — | S |
| Team "Share {team}" dialog | BA/CommunityCatalog.tsx:302-336 | DONE (ui/agents/team_dialogs.rs, ui/agents/share.rs) | — | S |

## 12. Teams (`BA/Team*Dialog.tsx`, `FA/team-*.ts`; native `ST/agents.rs:1676-1823`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Team snapshot format (size limits, `buzz_team_snapshot` chunk, checksum errors) | FA/team-encoding.ts:1-93 | DONE (ui/agents/team_file.rs) | — | L |
| "Import team snapshot" dialog: members, "Restore member memories", "Respond-to allowlist" Clear/Keep, "Creating team…", "Team imported" | BA/TeamImportDialog.tsx:40-157 | DONE (ui/agents/team_dialogs.rs) | — | M |
| Resumable team import (receipt, profile publish, team record) | FA/team-bundles.ts:27-101; FA/team-import.ts:9-120 | DONE (ui/agents/team_ops.rs, relay/src/session/teams.rs) | — | L |
| "Export {team}" dialog (memories, confirmation, JSON/PNG, `.team.{fmt}`) | BA/TeamExportDialog.tsx:11-156 | DONE (ui/agents/team_dialogs.rs) | — | M |
| "Deploy team to channel" dialog and behaviour (apply instructions, restart if stale, publish profile, add to channel, start; per-member failures) | BA/TeamDeployDialog.tsx:16-122; FA/team-deployment.ts:24-98 | DONE (ui/agents/team_dialogs.rs, ui/agents/team_ops.rs) | — | M |
| Team direct share, catalog switch, export | BA/DirectShare.tsx:232-314 | DONE (ui/agents/team_dialogs.rs, ui/agents/share.rs) | — | S |
| Native team instructions, capture, export, preview | ST/agents.rs:1676-1823 | DONE (host/teams.rs) | Host requests | M |

## 13. Old agent library and discovery (`BA/AgentLibrary.tsx`, `FA/library.ts`, `relay-library.ts`, `inventory.ts`, `known.ts`, `choices.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Other agents (N)" disclosure, identity rows by name, "No visible identities in your Buzz library." | BA/AgentLibrary.tsx:41-123 | DIFFERENT (kept): buzz-app shows this library only without desktop control; the port lists the same identities in the page groups (ui/agents/inventory.rs) | Only with a no-host body | M |
| "Profiles without identities" list | BA/AgentLibrary.tsx:124-144 | DONE (ui/agents.rs, ui/agents/discovery.rs) | — | S |
| Library states ("Reading agent inventory…", cleared, connect, error) and footer disclaimers | BA/AgentLibrary.tsx:76-96, 145-155; FA/library.ts:76-84 | DIFFERENT (kept): "Checking community inventory…" and the library error show on the page (ui/agents.rs); the no-host body and its disclaimers are not built | Only with a no-host body | S |
| Relay inventory read (kinds 30175/30177, owner only, avatar inheritance) | FA/relay-library.ts:1-129; FA/library.ts:24-42 | DONE (ui/agents/relay.rs) | — | M |
| Merge of local and relay sources with partial-failure messages | FA/inventory.ts:1-88 | DONE (ui/agents/relay.rs, ui/agents/discovery.rs) | — | S |
| Per-community inventory request and profile lookup across memberships | BA/use-community-inventory.ts:1-151; BA/community-profiles.ts:1-44 | DONE (ui/agents/discovery.rs) | — | M |
| Known-agent keys (profile flag plus library) | FA/known.ts:1-16; FA/use-known.ts | DONE (ui/identity.rs `AgentLibrary`, used by mentions and ui/members.rs) | — | S |
| Agent choices (managed + legacy, archived excluded, templates) | FA/choices.ts:1-252; FA/use-choices.ts | DONE (ui/templates/mod.rs `agent_choices`) | — | M |
| Public agent metadata (type and capabilities from 30177/10100) | FA/public-metadata.ts:1-95 | DONE (ui/profile_panel/agent.rs `public_metadata`) | — | S |

## 14. Identity archive and relay-only removal (`FA/AgentArchiveDialog.tsx`, `BA/inventory-archive.ts`, `BA/RelayAgentRemove.tsx`, `FA/relay-removal.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Archive {name}?" dialog (hides from search, mentions, Add member; "This agent is running. Archiving won't stop it.") | FA/AgentArchiveDialog.tsx:1-40 | DONE (ui/agents/dialogs.rs) | — | M |
| Archive/Unarchive request per identity; toast "{name} archived in {community}" with Undo; Undo notices retired when the view or community changes | BA/inventory-archive.ts:11-91; BA/UnifiedInventory.tsx:150-190 | DONE (ui/agents/discovery.rs, ui/agents/relay.rs, ui/agents.rs) | — | M |
| Relay-only "Remove" button and "Remove {name}?" dialog with three bullets | BA/RelayAgentRemove.tsx:39-85 | DONE (ui/agents/dialogs.rs, ui/agents/card.rs) | — | S |
| Removal: archive, delete 30177 (NIP-09), best-effort channel removal; "Record deletion is not confirmed. Retry." | FA/relay-removal.ts:14-213 | DONE (ui/agents/relay.rs, ui/agents/discovery.rs) | — | M |

## 15. Agent-requested updates (`BA/AgentUpdateReview.tsx`, `FA/management-request.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Listen for `agent_management_request` (create/update), queue capped at 200 | FA/management-request.ts:1-105; BA/AgentUpdateReview.tsx:74-88, 323-332 | DONE (ui/agents/review.rs `listen`; relay `Session::observe_management`) | — | M |
| Membership check; toast "Could not verify request membership" + Retry | BA/AgentUpdateReview.tsx:89-137, 189-201, 334-344 | DONE (ui/agents/review.rs) | — | S |
| Toasts "Agent creation needs attention… Create the agent yourself", "Could not load personal agents", "Agent update needs attention" | BA/AgentUpdateReview.tsx:203-253 | DONE (ui/agents/review.rs) | — | S |
| Editor pre-filled with the requested changes: "Requested by an agent. Review every field before saving."; writes paused until membership is confirmed | BA/AgentUpdateReview.tsx:254-285, 346-376 | DONE (ui/agents/review.rs, ui/agents/edit.rs) | — | M |

## 16. Mention wake (`FA/mention-wake.ts`, `FA/AgentWakeNotice.tsx`, `FA/control.ts:840-914`, `ST/agents.rs:559-586, 1103-1126`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Sending a message that p-tags your own stopped agent starts it with a replay floor; a waiting or starting agent gets the mention attached; Stop cancels the wake | FA/mention-wake.ts:1-88; FA/control.ts:840-914; ST/agents.rs:559-586, 1103-1126, 1150-1240 | DONE (ui/agents/wake.rs, host/start.rs) | — | M |
| "Agent could not start" toast ("Message sent, but X could not start…" plus variants; "Dismiss agent notice") | FA/AgentWakeNotice.tsx:1-19; FA/control.ts:848-912 | DONE (ui/agents/wake.rs) | — | S |

## 17. Agents2, attention, activity, thinking badge

### 17-A. Agents2 page (`src/bundled/agents2/*`, `src/features/agents2/*`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Agents2" primary page (`buzz.agents2`) | src/bundled/agents2/index.tsx:1-14; src/bundled/index.ts:175-178 | DONE (ui/agents2/mod.rs, app/src/agents2/service.rs, host/app_agents.rs) | Page, nav, and Agents2 service with native key custody | L |
| Header: "Agents2" or the agent's name; back "All agents"; "New agent" | src/bundled/agents2/Agents2Page.tsx:169-199 | DONE (ui/agents2/page.rs) | — | S |
| Unavailable, error ("Agents could not load") and empty ("No agents yet", two variants) states | Agents2Page.tsx:88-103, 143-161 | DONE (ui/agents2/page.rs) | — | S |
| Tile grid (10rem): avatar, name, type summary or "Type unavailable"; dimmed when the type is missing | Agents2Page.tsx:52-60, 263-290 | DONE (ui/agents2/page.rs) | — | M |
| Listbox keyboard model (arrows by column, Home/End, Enter opens Build, Escape deselects); click selects, double-click opens | Agents2Page.tsx:202-276 | DONE (ui/agents2/page.rs) | — | M |
| Side panel (22rem, animated, reduced motion respected) | Agents2Page.module.css:2-27, 156-181 | DONE (ui/agents2/page.rs) | — | M |
| Peek: avatar, "type · summary", Close, "Open", type Peek slot, "Enable the plugin…" note, "Wakes on" summary | Agents2Page.tsx:318-370; src/features/agents2/AttentionPanel.tsx:73-101 | DONE (ui/agents2/page.rs) | — | M |
| New agent form: Name, Type radio cards, "Create and set up", error copy | Agents2Page.tsx:372-458; src/features/agents2/service.ts:339-363 | DONE (ui/agents2/page.rs) | — | M |
| Build view: identity rail (avatar, inline rename, Type, Key + copy "Copied"), "Delete agent", tab host (type tabs then "Attention") | Agents2Page.tsx:460-574; src/bundled/agents2/AgentTabs.tsx; src/features/agents2/tabs.tsx | DONE (ui/agents2/page.rs, ui/agents2/tabs.rs) | — | M |
| Port-only: picture editing on the Build view ("Upload picture"/"Change picture", "Remove"), uploaded to the agent's community and published in its profile | n/a (buzz-app's Agents2 agents have no picture; profile content is `name` and `bot`) | DIFFERENT (kept): an addition of ours. The host saves the picture beside the identities and publishes it with the name (host/app_agents/pictures.rs, `AppAgentRequest::SetPicture`); hosts from before it answer "Update the agent's host to change its picture." (ui/agents2/page.rs) | Nothing | S |
| Delete dialog "Delete {name}?" and background cleanup (leave channels, archive, forget; retried on reconnect) | src/features/agents2/AgentManage.tsx:13-75; service.ts:493-597 | DONE (ui/agents2/manage.rs, app/src/agents2/cleanup.rs) | Cleanup reads a relay with no NIP-IA snapshot yet as "not archived" (buzz-app waits for one). | M |
| Agents2 tabs inside the profile panel and "Delete agent" there | src/bundled/profiles/ProfilePanel.tsx:181-190, 425-492 | DONE (ui/agents2/mod.rs `profile_sections`, hooked in ui/workspace/companion.rs) | Only the viewer's own profile opens in the companion panel today, so the hook shows once the profile panel (channels.md) opens other people's profiles there. | M |

### 17-B. Attention editor (`src/features/agents2/AttentionPanel.tsx`, `attention.ts`, `store.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Intro and the always-on "Mentions and replies" row | AttentionPanel.tsx:152-164 | DONE (ui/agents2/attention.rs) | — | S |
| Interest groups: title from slug, instructions, menu Edit/Remove (Remove guarded while used), inline edit, empty copy, "Add watch" / "Add timer" | AttentionPanel.tsx:230-384, 557-583 | DONE (ui/agents2/attention.rs) | — | M |
| Watch rows: icon, title, subtitle, on/off switch, "Edit JSON", Remove | AttentionPanel.tsx:416-515 | DONE (ui/agents2/attention.rs) | — | M |
| Timer rows: next run, ran X of N, until, "Ended", "Done…", "Run again" | AttentionPanel.tsx:386-414, 477-499; attention.ts:388-423 | DONE (ui/agents2/attention.rs) | — | M |
| "Without an interest" and "Skipped" sections | AttentionPanel.tsx:180-212, 517-555; store.ts:43-103 | DONE (ui/agents2/attention.rs, app/src/agents2/store.rs) | — | M |
| JSON editor with full spec validation | AttentionPanel.tsx:586-633; attention.ts:68-243 | DONE (ui/agents2/attention.rs, app/src/agents2/attention.rs) | — | M |
| New interest, watch (Wake on, In, channels, text, replies, advanced filter grammar) and timer (Prompt, Every, Stop after, Until) forms | AttentionPanel.tsx:636-977; attention.ts:245-339 | DONE (ui/agents2/attention.rs, app/src/agents2/attention.rs) | "Until" is a `YYYY-MM-DD HH:MM` text field: there is no date-time picker in the design system yet. | M |
| Limits (100 interests / 100 watches+timers) and persistence `buzz.agents2.v1` | store.ts:38-173 | DONE (app/src/agents2/store.rs) | — | S |

### 17-C. Agents2 runtime (`src/features/agents2/service.ts`, `native.ts`; `ST/app_agents.rs`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Agent-type registry (title, description, defaults, summary, Peek, tabs, run, timeout ≤30 min) | service.ts:135-154, 308-327 | DONE (app/src/agents2/registry.rs, app/src/agents2/runtime.rs) | Extension mechanism | L |
| Native custody: `app_agent_list` | ST/app_agents.rs:149-156 | DONE (host/app_agents.rs) | — | S |
| Native custody: `app_agent_create` (generate key, owner attests, save atomically) | ST/app_agents.rs:157-185 | DONE (host/app_agents.rs) | — | M |
| Native custody: `app_agent_rename` | ST/app_agents.rs:186-195 | DONE (host/app_agents.rs) | — | S |
| Native custody: `app_agent_delete` / `app_agent_forget` | ST/app_agents.rs:196-222 | DONE (host/app_agents.rs) | — | S |
| Native custody: `app_agent_publish` (bounded agent-signed event) | ST/app_agents.rs:223-235 | DONE (host/app_agents.rs) | — | S |
| Native custody: `app_agent_publish_profile` | ST/app_agents.rs:236-267 | DONE (host/app_agents.rs) | — | S |
| Native custody: `app_agent_query` (agent-authenticated relay read) | ST/app_agents.rs:268-313 | DONE (host/app_agents.rs) | — | S |
| Native custody: `app_agent_upload` (agent-signed media upload) | ST/app_agents.rs:314-361 | DONE (host/app_agents.rs) | — | S |
| Native custody: `app_agent_remember` (memory write) | ST/app_agents.rs:362-400 | DONE (host/app_agents.rs) | — | S |
| Scoped to the selected community and owner; reload after changes | service.ts:442-488, 603-655 | DONE (app/src/agents2/service.rs) | — | M |
| Wake on mentions, DMs and replies; event watches; timers; never hears itself | attention.ts:341-386; service.ts:680-800 | DONE (app/src/agents2/service.rs, app/src/agents2/attention.rs) | — | M |
| Run queue (one per agent, queue 32, 60/min, deadline), same-owner loop guard, profile sync retried | service.ts:179-188, 490-597, 722-880 | DONE (app/src/agents2/service.rs) | — | M |

### 17-D/E/F. Agent Activity panel, transcript and data layer (`src/bundled/agent-activity/*`, `FA/activity*.ts`, `FA/observer.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Observer telemetry (owner-encrypted kind 24200, decrypted, leased) | FA/activity.ts:80-139, 451-533; FA/observer.ts:1-58 | DONE (relay/src/session/observer.rs, app/src/agent_activity/observer.rs) | Prerequisite for everything below | L |
| "Agent Activity" panel and `buzz:agent-activity?…` targets; plugin lease | src/bundled/agent-activity/index.tsx:1-33; FA/activity-target.ts:1-53 | DONE (app/src/ui/agent_activity/mod.rs, app/src/agent_activity/mod.rs) | The plugin is compiled in, so its lease is held for the session | M |
| Panel states: connect, host cannot decode, "Retry live feed", empty "No captured activity yet…" | src/bundled/agent-activity/ActivityPanel.tsx:37-42, 389-410 | DONE (app/src/ui/agent_activity/panel.rs) | — | S |
| Agent select and conversation select (channel, thread with first prompt line) | ActivityPanel.tsx:64-177, 413-464 | DONE (app/src/ui/agent_activity/panel.rs) | — | M |
| Status line ("N observed working turn(s)." etc.), Transcript/Raw tabs, notices, diagnostics | ActivityPanel.tsx:187-285, 465-483 | DONE (app/src/ui/agent_activity/panel.rs) | — | S |
| Saved-history paging ("Show latest saved activity", "Load older activity") | ActivityPanel.tsx:204-221, 277-283, 355-369; FA/activity.ts:393-450 | DONE (app/src/agent_activity/observer.rs, through buzz-desktop's archive) | Archive host | M |
| Raw view accordion with event JSON and limits | ActivityPanel.tsx:286-341; FA/activity-records.ts:16-51 | DONE (app/src/ui/agent_activity/panel.rs, app/src/agent_activity/mod.rs) | — | M |
| Transcript: turn header (config, status, facts), "The start of this turn isn't loaded.", error and "Working…" rows | src/bundled/agent-activity/ActivityTranscript.tsx:56-169; FA/activity-transcript.ts:180-195 | DONE (app/src/ui/agent_activity/transcript.rs) | — | M |
| Transcript items: system prompt sections, prompts (steer), thoughts, Markdown agent reply, tool calls (kind icon, permission badge, diffs, output, elision), plan checklist, status items, chunk merging | ActivityTranscript.tsx:174-379; FA/activity-transcript.ts:92-514 | DONE (app/src/agent_activity/transcript_model.rs, app/src/ui/agent_activity/transcript.rs) | — | M |
| Turn state machine (working ≤30s, ended, unknown); bounded live window; access filtering; working-channel set | FA/activity.ts:15-321, 380-389, 503-604 | DONE (app/src/agent_activity/turns.rs, app/src/agent_activity/mod.rs) | — | M |
| Typing as working evidence (kind 9/20002 from agents, 8s) | FA/activity.ts:619-698 | DONE (app/src/agent_activity/turns.rs): from the session's typing state (20002 pulses a message ends), only for agents that send telemetry | Join with known agents | S |

### 17-G/H. Current-activity indicators and thinking badge

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Live activity labels ("Working", "Waiting for permission", "Reading {file}", "Running N tools"…) | FA/current-activity.ts:6-139 | DONE (app/src/agent_activity/current.rs) | — | S |
| Shared `AgentActivityRow` (avatar, name, duration, scope, "View activity") | FA/AgentActivityRow.tsx:9-83 | DONE (app/src/ui/agent_activity/row.rs) | — | S |
| Composer indicator separating "X is working" from "X is typing", with avatar stack | src/features/messages/TypingIndicator.tsx:30-228 | DONE (app/src/ui/typing.rs): channel and thread composers; observed turns and typing both count | — | M |
| Working-agents hover popover from the composer indicator (scopes, open thread, View activity, keyboard) | TypingIndicator.tsx:110-165, 230-340 | DONE (app/src/ui/typing.rs, app/src/ui/agent_activity/row.rs): rows with live labels, scope, open conversation and View activity; Escape closes; Down Arrow preview and single-agent click-to-open apply where clicks open panels (`AgentChooser::click_opens_panel`, buzz-app's personal workspace, which the port has no page for yet) | — | M |
| Sidebar channel "working" badge | src/bundled/channels/ChannelSidebarItem.tsx:83-119, 240-252 | DONE (app/src/ui/sidebar/rows.rs, app/src/ui/agent_activity/mod.rs) | — | M |
| Channel activity popover "Working now" | src/bundled/channels/ChannelActivityPopover.tsx:40-46, 197-330 | DONE (app/src/ui/channel_activity.rs): "Working now" rows with ticking durations, the triggering thread and View activity | — | M |
| Profile "Latest activity" preview | src/bundled/profiles/ProfileActivity.tsx:11-92 | DONE (app/src/ui/agent_activity/profile.rs): on the profile card | — | S |
| Thinking badge on squircle agent avatars (message rows, profile, Agents card), morph animation, a11y ", thinking" / ", available" | FA/AgentAvatar.tsx:20-101; src/shared/design-system/ui/agent-thinking/ThinkingBadge.tsx:7-157 | DONE (ui/src/components/thinking.rs, avatar.rs): message rows and the Agents card | The profile card's avatar is drawn by the profile slice | M |

## 18. Sessions / Me (`src/features/sessions/*` = FS, `src/bundled/sessions/*` = BS)

The port's header has a "Me" tab with no handler (app/src/ui/workspace.rs:385). The port has no `Page::Me` (app/src/ui/sidebar.rs:13-18).

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Legacy "Sessions (now in Me)" page redirects to Me ("Opening Me…") | BS/index.tsx:6-42 | DONE (crates/app/src/ui/me/mod.rs) | Route alias | S |
| Me workspace: "Session history" aside with sections nav, session column | BS/SessionsWorkspace.tsx:40-99; BS/SessionsPage.tsx:73-255 | DONE (crates/app/src/ui/me/mod.rs, crates/app/src/ui/me/sections.rs) | Me page | L |
| Connection states: "Connecting to your community…", "Connect to a community to work with your agents.", error | BS/SessionsPage.tsx:100-120 | DONE (crates/app/src/ui/me/mod.rs) | — | S |
| "Loading Me placement…", "Loading your conversation…", "This conversation is in Messages." | BS/SessionsPage.tsx:210-220, 450-462 | DONE (crates/app/src/ui/me/mod.rs, crates/app/src/ui/me/board.rs) | — | S |
| Section list: "Conversations" / "Sessions" default section, "New conversation" / "New session", "Sections couldn't load." | BS/SessionSections.tsx:140-200 | DONE (crates/app/src/ui/me/sections.rs) | — | M |
| Section menu: "Session settings…", "Delete section" (+ error) | BS/SessionSections.tsx:215-250 | DONE (crates/app/src/ui/me/sections.rs) | — | S |
| "New session in {section}" button | BS/SessionSections.tsx:280-290 | DONE (crates/app/src/ui/me/sections.rs) | — | S |
| Session row with actions menu (button, right-click, ContextMenu key) | BS/SessionSections.tsx:295-300, 600-650 | DONE (crates/app/src/ui/me/sections.rs; button and right-click, no ContextMenu key) | — | S |
| Session menu: "Share…", "Rename", Move to / Section submenu (+ "New group…" / "New section…"), Copy submenu (name, ID, link) and copy notice, "Session settings…" | BS/SessionSections.tsx:300-400, 474 | DONE (crates/app/src/ui/me/sections.rs; GPUI's clipboard write cannot fail, so there is no copy error notice) | — | M |
| "No sessions in this section." | BS/SessionSections.tsx:406 | DONE (crates/app/src/ui/me/sections.rs) | — | S |
| Move pending: "Saving section…", "Retry move", "Dismiss" | BS/SessionSections.tsx:445-470 | DONE (crates/app/src/ui/me/sections.rs) | — | S |
| "Create new section" dialog with "Section name" | BS/SessionSections.tsx:530-570 | DONE (crates/app/src/ui/me/dialogs.rs) | — | S |
| Session heading (channel name, Archived badge) and New session view titles | FS/SessionPresentation.tsx:10-95 | DONE (crates/app/src/ui/me/board.rs, crates/app/src/ui/me/new_session.rs) | — | S |
| Session actions menu: "Rename", "Session settings…" | BS/SessionsPage.tsx:383-416 | DONE (crates/app/src/ui/me/board.rs) | — | S |
| Tab pane toggle "Open tab pane" / "Close tab pane" | BS/SessionsPage.tsx:417-425 | DONE (crates/app/src/ui/me/board.rs) | — | M |
| Session agent control in the header | FS/SessionAgentControl.tsx:7-58 | N/A: SessionAgentControl is not mounted anywhere in buzz-app | — | S |
| AgentChoice picker: "Search agents", arrow/Enter keys, presence and admission labels, "No matching agents.", "Loading agents…", "No agents available in this community.", "Change agent: {name}" | FS/AgentChoice.tsx:26-388 | N/A: only SessionAgentControl uses it, and nothing mounts that | — | M |
| New-session composer "Message your agents" / "Message this session"; draft kept when sending is unavailable; actions menu; start errors | FS/NewSessionComposer.tsx:31-477 | DONE (crates/app/src/ui/me/new_session.rs) | — | L |
| Pending session start survives restart ("Your session start couldn't be saved on this device…") | FS/pending-start.ts; FS/NewSessionComposer.tsx:90-140 | DONE (crates/app/src/ui/me/pending.rs) | — | M |
| Recipient rules: "Session participants are still loading…", "There are multiple agents in this session. @mention who should respond." | FS/recipients.ts:30-42 | DONE (crates/app/src/ui/me/recipients.rs, crates/app/src/ui/channel.rs) | — | S |
| Personal ("Me") vs shared classification and metadata | FS/personal.ts; FS/metadata.ts | DONE (crates/relay/src/kit.rs, crates/relay/src/session/kit.rs) | — | S |
| "Rename session" dialog (1–120 chars, permission notice) | FS/RenameSession.tsx:19-151 | DONE (crates/app/src/ui/me/rename.rs) | — | S |
| "Session settings" (section defaults): "Start from a template" / "Custom settings", "Full Canvas", stale-draft notice, "Discard draft and reload saved settings", "Retry settings", "Loading settings…" | FS/WorkspaceSettings.tsx:29-293 | DONE (crates/app/src/ui/me/settings.rs) | — | M |
| Workspace fields: Project folders (≤20), "Work in a new worktree", Source repository + Browse, Worktree location, Base branch, Canvas | FS/WorkspaceFields.tsx:11-167 | DONE (crates/app/src/ui/me/settings.rs, crates/app/src/ui/me/workspace.rs) | — | M |
| Draft session settings dialog (Canvas before start) | FS/DraftSessionSettings.tsx:16-120 | DONE (crates/app/src/ui/me/settings.rs) | — | S |
| Workspace validation and section-default errors (24 KiB Canvas, invalid metadata, section removed…) | FS/workspace.ts:20-310 | DONE (crates/app/src/ui/me/workspace.rs, crates/app/src/ui/me/settings.rs) | — | S |
| Open a session at a selected message ("Selected message", loading, "could not be opened") | FS/SessionMessageTarget.tsx:36-214 | DIFFERENT (crates/app/src/ui/workspace.rs): a linked message outside the window opens in an exact thread panel beside the conversation, not inline with "Back to latest" | — | S |
| Share dialog "Share session" / "Share conversation": "Copy link", "Share to" (+ "Create new channel…"), Private channel, people search, "Who has session access" (Everyone / Selected people), Private vs public with "Make public and share", progress steps, errors | BS/SessionShare.tsx:57-1143; FS/share.ts:30-308 | DONE (crates/app/src/ui/me/share.rs, crates/relay/src/session/share.rs; no record of an unconfirmed channel creation, so its notice is left out) | — | L |
| Share-attempt recovery ("This share is already in progress in another view.") | FS/share-attempt.ts; BS/SessionShare.tsx:278-283 | DONE (crates/relay/src/session/share.rs) | — | S |
| Session link targets | BS/SessionsPage.tsx:568-578 | DONE (crates/app/src/ui/channel.rs links go through the workspace; a panel link opens on the Messages board, not Me's tab pane) | — | S |

## 19. Workflows (`src/bundled/workflows/*` = BW, `src/features/workflows/*` = FW)

The port renders workflow-authored messages only (app/src/notifications.rs:180-190; app/src/ui/message.rs:54).

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Workflows" primary page and nav entry | BW/index.tsx:3-13 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | Page::Workflows | M |
| Relay workflow protocol, queries, HTTP trigger, types, capability probe | FW/protocol.ts; FW/queries.ts; FW/http.ts; FW/types.ts; FW/host.ts; FW/capability.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | Relay-crate support | L |
| Page states: "Connecting to your community…", "Connect to a community to browse workflows.", "Workflow operations are unavailable from this host." | BW/WorkflowsPage.tsx:40-50, 95-105 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Channel filter ("All workflows", Community channels), "Reading channels…", errors, partial-list note, "Refresh workflows" | BW/WorkflowsPage.tsx:155-210 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Landing cards with trigger presentation (message, reaction + emoji, diff, schedule summary, webhook) | BW/WorkflowLanding.tsx:440-620; BW/workflowCardPresentation.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Card enable switch with "Turn on this workflow?" and frequency warnings ("This workflow may run often") | BW/WorkflowLanding.tsx:520-535, 640-655; BW/workflowActivationWarning.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Card menu: "Edit workflow" / "View workflow", "Run now", Delete; read-only reasons | BW/WorkflowLanding.tsx:440-600 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Delete confirmation, "Deleting…", "Deletion unconfirmed" | BW/WorkflowLanding.tsx:600-640 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Landing status ("Reading workflows…", discovery paused, "Workflows loaded.") and "New workflow" | BW/WorkflowLanding.tsx:785-830 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Editor dialog: "Create/View/Edit workflow", "Close editor", actions menu, Form/YAML mode switch and switch-blocked message | BW/WorkflowEditor.tsx:110-240 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | L |
| Inline workflow name edit (Enter/Escape) | BW/WorkflowEditor.tsx:280-335 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| "Enabled in configuration" switch and "Save this workflow enabled?" | BW/WorkflowEditor.tsx:360-370, 470-482 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| YAML editor ("Original text is kept until you edit… Schedules use UTC.") | BW/WorkflowEditor.tsx:400-410; BW/workflowYamlDocument.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| "Workflow settings & activity" accordion with Description | BW/WorkflowEditor.tsx:450-460 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Form flow: trigger node, step nodes, "Add step" / "Add after Step N", inspector, "Remove step" | BW/WorkflowForm.tsx:140-480 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | L |
| Triggers: Message posted, Reaction added (Emoji), Diff posted, Schedule, Webhook | BW/WorkflowForm.tsx:50-60, 240-320 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Actions: Send Message (text, destination, reply in thread), Delay, Call Webhook | BW/workflowFormTypes.ts:79-81; BW/WorkflowForm.tsx:520-650 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Conditions: Basic/Advanced, operators, Add/Remove, evalexpr, "Replace with basic filters?" | BW/WorkflowConditions.tsx:20-210; BW/workflowConditionExpression.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Schedule fields: Repeats, Repeat on, Day of month, Existing interval, warnings | BW/WorkflowScheduleFields.tsx:15-258; BW/workflowSchedule.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Cron expression input and parser | BW/CronExpressionInput.tsx; BW/cronExpression.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Webhook action fields: URL, HTTP method, Headers, Request body | BW/WorkflowWebhookFields.tsx:20-134 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Webhook secret: "Webhook ready" dialog with reveal and copy; "Continue without this secret?" | BW/WorkflowWebhookSecretDialog.tsx; BW/WorkflowWebhookSecrets.tsx | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Form/YAML parse errors ("… — use the YAML editor"); header uniqueness | BW/workflowFormTypes.ts:270-525; BW/WorkflowEditor.tsx:110-120 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Channel workflows view: "Saved configurations", "Refresh configurations", "New workflow", Configured enabled/disabled/Unreadable, "Read runs" / "Hide runs", "Run now", "Configuration details" | BW/WorkflowChannel.tsx:316-560 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Confirmations "Leave this draft?", "Run this workflow now?", "Delete this workflow?" | BW/WorkflowChannel.tsx:555-590; BW/ConfirmAction.tsx | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Runs: "Runs", "Refresh runs", states, "Run details and trace", Run ID, "Newest runs" / "Older runs" | BW/WorkflowRuns.tsx:50-124 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| "Recent activity" operations: pending/succeeded/rejected/unknown copy, "Delivery details", "Check saved configuration", dismiss confirmation | BW/WorkflowOperations.tsx:10-171 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | M |
| Deletion notices ("Deleting workflow…", "Couldn't delete…", "Checking deletion…") | BW/WorkflowDeletionNotices.tsx:20-164 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Channel picker combobox "Choose a channel" | BW/WorkflowChannelPicker.tsx:40-68 | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |
| Duration formatting | BW/workflowDuration.ts | DONE (crates/app/src/ui/workflows/, crates/relay/src/workflows/) | — | S |

## 20. Terminal (`src/bundled/terminal/*` = BT, `ST/terminal/*`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Per-channel "Terminal" companion panel | BT/index.tsx:66-72; BT/TerminalPanel.tsx | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | Dock and panel | L |
| Toolbar launcher "Toggle channel terminal" with shortcut in its title | BT/index.tsx:39-64 | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | — | S |
| Shortcut Mod+J "Toggle channel terminal" in shortcut settings | BT/index.tsx:14-35 | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | — | S |
| Header "BuzzTerm" with channel and thread labels; "Restart terminal", "End session", "Hide terminal (Cmd/Ctrl+J)" | BT/TerminalPanel.tsx:85-160 | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | — | S |
| Escape stays inside the terminal | BT/TerminalPanel.tsx:195 | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | — | S |
| Sessions per channel/thread kept while hidden; 20-session cap; input-full error | BT/sessions.ts:100-200 | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | — | M |
| Terminal renderer, theme from tokens, JetBrains Mono, resize | BT/renderer.ts; BT/appearance.ts | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | Terminal emulator widget | L |
| Welcome splash: "buzz" hex wordmark, pointer-reactive, reduced motion | BT/splash.ts; BT/banner.ts | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | — | M |
| Native PTY: login shell with TERM and BUZZ_CHANNEL_ID / BUZZ_CHANNEL / BUZZ_NPUB / BUZZ_RELAY_URL / BUZZ_TERM_SESSION / BUZZ_THREAD_ID; spawn, read, write, resize, close, close-owner; context validation | ST/terminal/mod.rs:200-324; ST/terminal/unix.rs:20-45; ST/terminal/context.rs | DONE (crates/app/src/ui/terminal/, crates/host/src/terminal.rs) | — | L |

## 21. Mobile pairing (`src/bundled/pairing/*`, `src/features/pairing/client.ts`, `ST/pairing/*`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Settings card "Pair mobile" (Account group) | src/bundled/pairing/index.tsx:3-14 | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | S |
| Account gate: "Use existing Buzz account", mismatch error, dev-broker note, desktop-only note | src/bundled/pairing/PairingSettings.tsx:90-206 | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | S |
| Community address input ("Enter your community's https:// or wss:// address.") | PairingSettings.tsx:105-115, 220-230 | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | S |
| Steps "Scan QR code", "Enter code on your phone" / "Confirm mobile code", "Pair your mobile app" / "Paired" | PairingSettings.tsx:145-170, 365-400 | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | M |
| QR display (SVG with reveal) | PairingSettings.tsx:255-262; ST/pairing/qr.rs; ST/pairing/qr-reveal.css | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | M |
| Six-digit code entry or comparison | PairingSettings.tsx:263-300 | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | S |
| End states "Check your phone" + "Start a new pairing", "Phone paired" + "Pair another phone"; expired, uncertain, error, cancelled | PairingSettings.tsx:330-356; ST/pairing/mod.rs:20-40 | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | S |
| Client polling and cancel errors | src/features/pairing/client.ts:100-177 | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | S |
| Native pairing session over relay: start, status, confirm, deny, cancel, account | ST/pairing/mod.rs:160-577; ST/pairing/relay.rs; ST/pairing/identity.rs | DONE (crates/pair-mobile, crates/app/src/ui/settings/pairing/) | — | L |

## 22. Bestie (`src/bundled/bestie`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Bestie" primary page with placeholder "Meet your Bestie" / "Your companion's home in Buzz. Agent chat isn't connected yet." | src/bundled/bestie/index.tsx:9-53 | DONE (crates/app/src/ui/bestie.rs; off by default, Settings → Plugins turns it on) | Page and nav icon | S |

## 23. Builderlab (`src/bundled/builderlab/**` = BB)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Settings → Integrations → "Builderlab" card | BB/index.tsx:36-60 | SKIPPED (decided: Builderlab) | — | S |
| Login: "Sign in with Builderlab", "Finish sign-in in your browser…", "Cancel sign-in", "Signed in as {email}.", "Sign out", unavailable reason | BB/login/Login.tsx:9-75 | SKIPPED (decided: Builderlab) | — | M |
| OAuth PKCE through the loopback callback, code exchange, `/v1/auth/me` check | BB/oauth/browser.ts; BB/oauth/session.ts | SKIPPED (decided: Builderlab) | — | M |
| "Remote agents" list ("No remote agents yet.", Refresh / Retry) | BB/agents/RemoteAgents.tsx:278-465 | SKIPPED (decided: remote agents via Builderlab) | — | M |
| Create remote agent (name, optional instructions, community or Personal space), enrollment (attest, kind-30177 registration), "Finish setup", "Retry community setup" | BB/agents/RemoteAgents.tsx:285-330; BB/agents/enrollment.ts; BB/agents/registration.ts | SKIPPED (decided: remote agents via Builderlab) | — | L |
| Edit instructions on Active agents | BB/agents/RemoteAgents.tsx:382-430 | SKIPPED (decided: remote agents via Builderlab) | — | S |
| "Delete {name}?" (kind-5 deletion, Beekeeper delete, tombstones) | BB/agents/RemoteAgents.tsx:465-486; BB/agents/enrollment.ts | SKIPPED (decided: remote agents via Builderlab) | — | M |

## 24. OAuth loopback callback (`ST/oauth_callback/*`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `oauth_callback_begin` / `wait` / `cancel`: 127.0.0.1 ephemeral listener, injected `redirect_uri`, 256-bit single-use state, opens browser, timeout | ST/oauth_callback/mod.rs:177-215; ST/oauth_callback/README.md | DONE (crates/app/src/oauth.rs, via buzz_desktop::oauth_callback) | — | M |
| Callback HTML page "Sign-in callback" with strict headers | ST/oauth_callback/mod.rs:330-349 | DONE (crates/app/src/oauth.rs, via buzz_desktop::oauth_callback) | — | S |

## 25. Claude Code agent plugin (`src/agent-plugins/claude-code/*` = CC)

The Claude Code harness preset (claude-agent-acp) exists in the port (host/harness.rs:40-47). This plugin is a separate Agents2 type.

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Agents2 type "Claude Code" (one continuing session per thread; summary = model) | CC/index.ts:39-52 | DONE (app/agent_plugins/claude_code/mod.rs) | — | L |
| Claude tab: status ("Checking Claude Code…", "not installed", "needs you to sign in", "is ready (account)"), "Install Claude Code", "Sign in", "Cancel", "Check again", setup output, sign-in code + "Send" | CC/tabs.ts:100-225; CC/setup.ts | DONE (app/agent_plugins/claude_code/tabs.rs, setup.rs; host/processes.rs) | — | M |
| "Running sessions" list (Working/Idle; empty copy) | CC/tabs.ts:228-270 | DONE (app/agent_plugins/claude_code/tabs.rs) | — | S |
| Settings tab: Model (default/Opus/Sonnet/Haiku), Workspace, Sessions (per thread/channel), Answers (Only you/Anyone) + warning | CC/tabs.ts:20-31, 300-350 | DONE (app/agent_plugins/claude_code/tabs.rs) | — | S |
| Runtime: warmed spare session, live-session cap, memory read, base prompt, saved sessions | CC/runtime.ts; CC/sessions.ts; CC/prompt.ts; CC/base_prompt.md; CC/client.ts | DONE (app/agent_plugins/claude_code/{runtime,sessions,claude,prompt,client}.rs; app/buzz_mcp) | — | L |

## 26. Plugin system (`src/plugins/*`, `src/features/host/service.ts`; native `ST/host_process.rs`, `host_command.rs`, `host_request.rs`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Plugin manager: bundled and external plugins, enable/disable, activation with timeout, starting/active/failed status | src/plugins/manager.ts; src/plugins/runtime.ts; src/plugins/timeout.ts; src/plugins/status.ts | DONE for bundled plugins (crates/app/src/plugins.rs); external plugins are skipped by decision | - | L |
| Persisted plugin configuration (`buzzodz.plugins.v1`) and module loader | src/plugins/storage.ts; src/plugins/modules.ts | DONE (crates/app/src/plugins.rs: buzz-app plugin manager registry.json via buzzodz-plugins) | - | M |
| Settings → Plugins UI, plugin import from folder or Git, "External plugins are paused for this launch", recovery screen | src/app/Settings.tsx:219-320 (uses src/plugins) | DONE (crates/app/src/ui/settings/plugins.rs); import is skipped by decision | - | M |
| Contribution resolution (match order) and owned-contribution error boundary | src/plugins/contributions.ts; src/plugins/OwnedContribution.tsx | DONE (crates/app/src/plugins.rs gates pages, panels and renderers by plugin; panels resolve in registration order). No error boundary: Rust views cannot throw | — | M |
| Manifest host capabilities (commands, networkOrigins, processes) and the public author API | src/plugins/api.ts:6-42; src/plugins/author.ts; src/plugins/types.ts | N/A: external plugins are skipped (GAPS.md, Decided) | — | M |
| Host service for plugins (request, run command, spawn) | src/features/host/service.ts | N/A: only external plugins and the skipped Builderlab and Hosted communities use it | — | M |
| `plugin_host_process_spawn` / `write` / `kill` (streamed stdout/stderr, killed on reload or exit, identity stripped) | ST/host_process.rs:1-431 | N/A: only external plugins use it; agent processes go through the host API | — | M |
| `plugin_host_run_command` (bounded stdout, effective PATH) | ST/host_command.rs:178-300 | N/A: only external plugins and skipped bundles use it | — | S |
| `plugin_host_request` (HTTP to declared origins) | ST/host_request.rs:39-367 | N/A: only external plugins and skipped bundles use it | — | S |
| Trusted plugin security service for agents (`agent_security`: register, default and per-agent policies) | FA/security.ts:1-115; ST/agents.rs:1650-1675 | N/A: no bundled plugin registers an agent security policy | — | M |

## 27. buzz-mcp (`src/buzz-mcp/*`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Standalone stdio MCP server (`BUZZ_RELAY_URL`, `BUZZ_PRIVATE_KEY`, `BUZZ_AUTH_TAG`, `BUZZ_CHANNEL`, `BUZZ_MCP_ROOT`); `dist-buzz-mcp` build | src/buzz-mcp/main.ts:1-53; src/buzz-mcp/node.ts; vite.buzz-mcp.config.ts | N/A: the port's agents use the in-process Buzz tools (crates/app/src/buzz_mcp); none runs the stdio build | — | M |
| Tool `send` (text/path, channel, reply, files, mentions, final) | src/buzz-mcp/tools.ts:28-45 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `react` | src/buzz-mcp/tools.ts:46-51 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `read` (thread, channel latest, search q) | src/buzz-mcp/tools.ts:52-55 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `channels` | src/buzz-mcp/tools.ts:56 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `members` | src/buzz-mcp/tools.ts:57 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `users` | src/buzz-mcp/tools.ts:58 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `dm` | src/buzz-mcp/tools.ts:59 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `mem_get` | src/buzz-mcp/tools.ts:60 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `mem_set` | src/buzz-mcp/tools.ts:61-63 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Tool `canvas` | src/buzz-mcp/tools.ts:64-68 | DONE (crates/app/src/buzz_mcp/tools.rs) | — | S |
| Client and RPC layer (`callTool`, error text) used by the Claude Code plugin | src/buzz-mcp/client.ts; src/buzz-mcp/rpc.ts | DONE (crates/app/src/buzz_mcp/rpc.rs) | — | S |

## 28. Native agent backend (`ST/agents.rs`, `ST/agents/*`, `ST/agent_models.rs`, `ST/goose_models.rs`, `ST/pi_models.rs` vs `host/`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `agent_control_save` / `agent_control_save_defaults` restart running agents whose effective settings changed (`restarted`, `restartFailures`) | ST/agents.rs:970-1070 | DONE (host/start.rs) | — | M |
| "Restart required" from the controller's `restartDiff` | FA/control.ts:15-25, 79-80; ST/agents.rs:1068-1070 | DONE (host/lib.rs, ui/agents/card.rs) | — | S |
| Start pipeline: credential request, "waiting" (Keychain), one pending start per agent ("Agent start already in progress; use Stop to cancel"), Stop cancels a waiting start, start tickets | ST/agents.rs:1150-1350 | DONE (host/start.rs) | — | M |
| Pi preflight `verify` before start | ST/pi_models.rs:37-150; ST/agents.rs:1240-1290 | DONE (host/start.rs) | — | S |
| Databricks OAuth credential wait at start; Disconnect cancels waiting and queued starts ("Start cancelled by Disconnect; reconnect and retry Start") | ST/agents.rs:816-853 | DONE (host/start.rs, host/models.rs) | — | M |
| Refuse start while old Buzz is running ("Stop old Buzz before starting agents here…") | ST/agents.rs:587-593, 1613-1648 | DONE (host/start.rs) | — | S |
| Mention attach and replay floor (`agent_control_attach_mention`, `action` with `replay_floor`) | ST/agents.rs:559-586, 1103-1126 | DONE (host/start.rs) | — | M |
| Harness log: `agent_control_log_challenge` + `agent_control_read_log` (owner-signed challenge) and the profile log viewer | ST/agents.rs:598-662, 937-961; src/bundled/profiles/ProfileHarnessLog.tsx | DONE (host/start.rs; ui/profile_panel/agent.rs `HarnessLog`) | — | M |
| `agent_control_use_here` | ST/agents.rs:1354-1365 | DONE (host/lib.rs) | — | M |
| `agent_control_local_clone_settings` / `agent_control_clone_settings` | ST/agents.rs:1366-1386 | DONE (host/lib.rs) | — | S |
| `agent_control_snapshot_memory_write` (restore memories from a snapshot) | ST/agents.rs:1595-1612; ST/agents/snapshot_memory.rs | DONE (host `Request::RestoreMemory`; ui/agents/snapshots.rs, ui/agents/team_ops.rs) | — | M |
| Agent-signed owner-addressed memory event publication | ST/agents/profile_http.rs:93-126 | DONE (host `Request::RestoreMemory`) | — | S |
| Team instructions, capture, export, preview | ST/agents.rs:1676-1823 | DONE (host `Request::TeamCapture`; ui/agents/team_ops.rs) | — | M |
| Databricks catalog `agent_models_run` connect/refresh (PKCE through `buzz_agent`, cache dir, build-default host/filter) | ST/agent_models.rs:20-31, 245-630 | DONE (host/models.rs (buzz-desktop agent_models)) | — | L |
| `agent_models_run` disconnect | ST/agent_models.rs:48-53; ST/agents.rs:816-853 | DONE (host/models.rs) | — | S |
| `agent_models_run` test (one tiny completion; Goose and Pi inference checks) | ST/agent_models.rs:52-53; ST/goose_models.rs:25-60; ST/pi_models.rs:154-275 | DONE (host/models.rs) | — | M |
| `agent_models_begin` / `agent_models_cancel` ticket admission (one lookup at a time, cancellable) | ST/agent_models.rs:245-260 | DONE (host/models.rs) | — | S |
| `claude_auth_status` (`claude auth status`) | ST/agents.rs:148-200 | DONE (host/setup.rs) | — | S |
| Snapshot fields: `harnessOptions` with `defaultArgs`, providers and `configurationPolicy` (authentication, model-with-provider) supplied natively | ST/agents.rs:306-470 | DONE (host/harness.rs) | — | M |
| Snapshot flags: `createAvailable`, `importAvailable`, `localInventoryActions`, `avatarEditingAvailable`, `parked`, `inventoryWarnings`, `databricksDefaults` | ST/agents.rs:526-545; FA/control.ts:120-160 | DONE (host/lib.rs) | — | S |

## 29. Harness setup, managed tools, detection (`ST/harness_setup.rs`, `ST/managed_npm.rs`, `ST/agents.rs` harness options)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `pi_install`: app-owned Node + npm install of Pi and buzz-pi-acp, install/update ("Install" / "Update Pi") | ST/harness_setup.rs:244-323; ST/managed_npm.rs:439-500 | DONE (host/setup.rs (buzz-desktop harness_setup); app/src/ui/settings/agents/harnesses.rs) | — | L |
| `claude_install`: managed Claude Code adapter; rejects the native stub | ST/harness_setup.rs:325-369; ST/managed_npm.rs | DONE (host/setup.rs) | — | M |
| Pinned Node artifacts with checksum, scrubbed npm env, prefix-link refusal, release shims, pruning | ST/managed_npm.rs:20-438 | DONE (host/setup.rs (buzz-desktop managed_npm)) | — | M |
| One install at a time ("A Harness installation is already in progress."); installer process group killed on Quit; install log; failure copy | ST/harness_setup.rs:19-200; FA/control.ts:587-615 | DONE (host/setup.rs, app/src/hosts.rs) | — | S |
| After a Pi install, start agents that were waiting for Pi | ST/harness_setup.rs:210-243; ST/agents.rs:796-815 | DONE (host/setup.rs, host/start.rs) | — | S |
| Detection prefers managed tools over user tools; statuses "Ready" / "CLI needed" / "Adapter needed" / "Unknown"; Pi update available | ST/agents.rs:258-305 | DONE (host/harness.rs; app/src/ui/settings/agents/harnesses.rs) | — | M |

## 30. buzz-app main of 2026-10-10 (fork rev `ec7d795f`)

Upstream changes since the previous pin. `CX/` = `src/bundled/codex/`;
`codex/` = `app/src/agent_plugins/codex/`.

### 30-A. Native Codex harness (#751 `89ee72a9`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Codex harness option: stable integration `id` on every option, status Ready / CLI needed / Adapter needed / Not enabled, install support on macOS and Linux | ST/agents.rs `harness_options` | DONE (host/harness.rs, host/api.rs `HarnessOption.id`) | — | M |
| Codex sign-in check (`codex login status` exit code) | ST/agents.rs `codex_auth_status` | DONE (host/setup.rs, host `Request::CodexAuth`) | — | S |
| Codex ACP adapter install (pinned `codex-acp@2.1.1`, app-owned Node) | ST/harness_setup.rs `codex_install`; ST/managed_npm.rs | DONE (host/setup.rs via buzz-desktop `install_codex`; `InstallHarness { Codex }`) | — | M |
| Codex model and effort discovery (`codex debug models`, Browse/Refresh only) | ST/agent_models.rs; ST/codex_models.rs | DONE (buzz-desktop `agent_models`/`codex_models`; host/api.rs `ModelRequest.integration`/`selected_model`, `Catalog.codex`) | — | M |
| Settings → Harnesses Codex row: logo, "Checking sign-in…" / "Ready" / "Sign-in needed" / "Sign-in unconfirmed", Install with progress, error and log, manual adapter command, CLI and `codex login` hints | src/app/AgentSettings.tsx | DONE (ui/settings/agents/harnesses.rs, app/src/hosts.rs) | Adds the host's `codex login` terminal command, as the Claude Code row does, for remote hosts | M |
| Codex blocks the Pi and Claude Code installs while it installs | src/app/ClaudeHarnessSetup.tsx; FA/control.ts | DONE (ui/settings/agents/harnesses.rs, app/src/hosts.rs) | — | S |
| Agent dialogs: Codex picked by integration, Default / Advanced configuration, model and effort pickers, unavailable saved choices, Provider and Arguments hidden, "(coming in a later update)", missing-Codex hint | BA/AgentHarnessEditor.tsx; BA/AgentSettingsFields.tsx; BA/CodexConfigurationFields.tsx; BA/harness-policy.ts | DONE (ui/agents/fields.rs, ui/agents/fields/codex.rs, ui/agents/draft.rs) | — | L |
| Codex draft validation messages and edit round trip | BA/agent-edit.ts | DONE (ui/agents/draft.rs) | — | S |
| Agent-requested runtime switch into or out of Codex | BA/AgentUpdateReview.tsx `requestedDraft` | DONE (ui/agents/review.rs) | — | S |
| "Saving changes…" while an edit saves | BA/AgentEditor.tsx | DONE (ui/agents/edit.rs) | — | S |
| Agent defaults note: Codex is configured per agent | src/app/AgentDefaultsCard.tsx | DONE (ui/agents/defaults.rs) | — | S |
| Codex brand mark and attribution | src/shared/design-system/icons/HarnessLogos.tsx; NOTICE.md | DONE (assets/icons/brand/codex.svg, ui/src/icon.rs, NOTICE.md) | — | S |

### 30-B. Codex agent type in Agents2 (`b23fb875`, #830, #831, #832, #826, #851)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Agents2 type "Codex" (`buzz.codex/codex`), macOS and Linux only, 2-minute hand-over | CX/index.ts | DONE (codex/mod.rs) | — | M |
| Declared host processes: `workspace` (`pwd`), `app-server` (`codex app-server`), `read` (`base64 -i`) | CX/manifest.json | DONE (host/processes.rs) | — | S |
| `codex app-server` JSON-RPC client: request correlation, chunked output, Buzz dynamic tools, refused interactive requests, model paging | CX/rpc.ts | DONE (codex/rpc.rs) | — | L |
| Runtime: one server per agent, one Codex thread per conversation, steering, idle follow-ups, saved bindings `buzz.codex.sessions.v3` (200), missing-thread recovery, interruptions, background terminal cleanup, 29-minute turns | CX/runtime.ts | DONE (codex/runtime.rs) | — | L |
| Shared Buzz app client for Claude Code and Codex | src/buzz-mcp/app-client.ts | DONE (app/src/buzz_mcp/app_client.rs, app/src/agent_plugins/owner_relay.rs) | — | M |
| Conversation history and turn input; thread names `Buzz #<channel> · thread <root> · <agent>`, client `buzz_codex_plugin` (#826) | CX/history.ts; CX/prompt.ts | DONE (codex/history.rs, codex/prompt.rs) | — | M |
| Codex tab: status, account, Check again, conversations with status and detail | CX/tabs.ts `CodexTab` | DONE (codex/tabs.rs) | Also shows the host's `codex login` command when signed out | M |
| Settings tab: Model ("Codex default · …", unavailable entries), Thinking, Workspace, Sessions, Instructions, Save | CX/tabs.ts `SettingsTab`; CX/config.ts | DONE (codex/tabs.rs, codex/config.rs) | — | M |
| Bundled and on by default; formerly installed copies migrate (#830) | crates/plugin-manager; src/bundled/index.ts | DONE (buzzodz-plugins at the pin; agents2/registry.rs gates types by plugin) | — | S |
| `danger-full-access` sandbox, approval `never` (#832) | CX/runtime.ts | DONE (codex/runtime.rs) | — | S |
| Default workspace `~/.buzz` (#831) | CX/config.ts; CX/runtime.ts | DONE (codex/config.rs, codex/runtime.rs) | — | S |
| Replies go to the thread, not under the mention (#851) | CX/prompt.ts; CX/runtime.ts | DONE (codex/prompt.rs, codex/runtime.rs) | — | S |
| Opt-in live check against the installed CLI | CX/live.mjs | DONE (codex/live.rs, `--ignored`) | — | S |
| Session viewer docs and export scripts (#826) | docs/view-codex-session.md; scripts/export-*-session.mjs | N/A: buzz-app developer tooling; it reads the rollouts this port's sessions write | — | — |

### 30-C. Other agent changes

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Claude Code agent type bundled and on by default; turning the plugin off hides its type (#818) | crates/plugin-manager; src/bundled/claude-code | DONE (buzzodz-plugins at the pin; agents2/registry.rs, agents2/service.rs) | — | S |
| Base prompt commit-attribution line (#833) | src/bundled/claude-code/base_prompt.md | DONE (agent_plugins/claude_code/base_prompt.md) | — | S |
| Each Agents2 agent runs in one copy of the app (#790) | ST/app_agents.rs; src/features/agents2/service.ts | DONE (host/app_agents.rs, host/app_agents/hosting.rs `Claim`/`Release`; agents2/service.rs) | — | M |
| Agents keep running while another community is selected (#838, ported ahead of merge): every joined community with one of the viewer's agents is bound with the selected one; each community's live events reach only its own agents; deleted-agent cleanup runs in unselected communities; a failed unselected community is retried every 30 s; `running` beside `agents`; `relay(pubkey)`; Claude Code and Codex keep processes for `running` and read through each agent's own community; one shared Claude Code spare and no per-agent process cap | src/features/agents2/service.ts; src/features/communities/service.ts; src/bundled/claude-code/{claude,runtime,sessions}.ts; src/bundled/codex/{index,runtime}.ts | DONE (agents2/service.rs, agent_plugins/owner_relay.rs `running`, claude_code/{claude,runtime,sessions}.rs `Spare`, codex/{mod,runtime}.rs) | No `Communities.open`: the port opens every joined community's session at launch. Leaving is the only "disconnected" state. The spare is also keyed by the host that would run it | L |
| Shared team instructions in their own `buzz-team-text-v1` record, chunked, owner-checked | src/features/agents/team-instructions.ts; src/features/channel-templates/*; ST/relay/kit.rs (#775) | DONE (relay/src/kit/mod.rs, relay/src/kit/payload.rs, relay/src/session/teams.rs) | — | L |
| Team save writes the text, then syncs every member's copy; delete and app start sync too; conflicting teams refused (#775) | src/bundled/channel-templates/ChannelTemplatesDialog.tsx; TemplateLibrary.tsx; ST/agents.rs `agent_control_team_sync` | DONE (ui/agents/team_text.rs, team_editor.rs, teams.rs; host `Request::TeamSync`) | Syncs each connected host that runs agents in the community | M |
| Deploy only adds and starts members; export, share, catalog and import carry the text (#775) | FA/team-deployment.ts; BA/TeamDeployDialog.tsx; BA/TeamExportDialog.tsx; FA/team-import.ts | DONE (ui/agents/team_ops.rs, team_dialogs.rs) | — | M |
| Bounded stdin for fixed plugin host commands (#769) | ST/host_command.rs | N/A: only external plugins use host commands (skipped by decision); the fork keeps the change in buzz-desktop `process::run_output_with_input` | — | — |
| Placeholder Builderlab agent type (#788) | src/bundled/builderlab | SKIPPED (decided: Builderlab) | — | — |

## buzz-app main of 2026-10-14

Upstream changes since the previous pin; the fork is pinned at `219fea82` (rebased on main `687985b5`). `BA/agents2/` is buzz-app's `src/bundled/agents2/` before #871 renamed it.

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Bring over: identities earlier agent systems saved (this app's v1 harness agents, old Buzz installed and development) listed per host, minus the viewer's Agents; taken over as an agent of the chosen type in the selected community with key, name, picture and description; source labels "Buzz 1.0 harness agent" / "Buzz 0.X" / "Buzz 0.X dev", npub suffix and old community; "Looking for agents…", "No other agents are saved on this machine.", store warnings (#858 `63f7e903`) | BA/agents2/Agents2Page.tsx `BringOver`; FA/agents2/service.ts `portable`/`port`; ST/app_agents.rs `app_agent_portable`/`app_agent_port`; crates/agent-controller port.rs | DONE (ui/agents2/bring_over.rs, ui/agents2/page.rs, agents2/service.rs `portable`/`port`/`add`; host/app_agents.rs; host api `AppAgentRequest::Portable`/`PortPrepare`, `AppAgentResponse::Portable`/`PortPrepared`) | The owner attests the taken-over key through the existing `CreateCommit`, so the human key stays in the GUI; rows from several hosts name their host | M |
| Picture and description as ordinary profile metadata in the identity, published with the name; `profile_is_current` (#858) | crates/agent-controller app_agent.rs; ST/app_agents.rs `app_agent_set_picture`, `app_agent_publish_profile` | DONE (host/app_agents.rs; host/app_agents/pictures.rs moves pictures this app kept beside the identities into them once) | — | S |
| Set the picture from the Build rail with the profile avatar editor (compact, squircle) (#858) | BA/agents2/Agents2Page.tsx `AgentBuild` | DONE (ui/agents2/page.rs `Build`, ui/settings/avatar/mod.rs `popover`) | Replaces this port's earlier Upload / Remove picture buttons (its DIFFERENT row); a host from before pictures still says to update it | S |
| Remove v1 agents: the Agents page and its create, edit, delete, import, snapshot, team, catalog, library and use-here dialogs; mention wake; agent-requested updates; snapshot previews in links, pastes and attachments (now ordinary files) (#859 `29c36a8b`) | src/bundled/agents/**; FA/{mention-wake,management-request,snapshot*,team-*,catalog*}.ts; src/features/messages/{MessageComposer,MessageRow,SnapshotAttachment}.tsx; src/features/conversation/MessageLink.tsx | DONE (removed app/src/ui/agents/** but form, host_dialog, inventory and relay archive reads; app/src/snapshot; ui/media/attachments.rs; ui/composer.rs; ui/links.rs; ui/channel.rs; ui/thread.rs; ui/workspace.rs) | Sections 1–16, 28 and 29 above describe the removed v1 port | L |
| Profile panel without v1: Info and Channels tabs; no Runtime, Memories, instance panels, harness log, public agent metadata, archive controls or Add to channel; Retry profile only for a missing profile (#859) | src/bundled/profiles/{ProfilePanel,ProfileChannels,ProfilePublicMetadata,index}.tsx | DONE (ui/profile_panel.rs, ui/profile_panel/agent.rs keeps "Managed by") | Agents sections stay inside Info (an earlier difference) | M |
| Local agents for names, mentions, typing, search and members come from Agents identities on every host (`LocalAgents`), owner's agents in their own community; choices carry no local picture (#859) | FA/local.ts; FA/choices.ts; src/features/identity-names/directory.ts; FA/agents2/service.ts `local` | DONE (agents2/service.rs `local`, ui/identity.rs `AgentLibrary::from_hosts`, ui/typing.rs, ui/sidebar/rows.rs, ui/search.rs, ui/members.rs) | — | S |
| Adding a member no longer starts an agent; "Membership changed…" and start errors gone (#859) | src/features/channel-members/{members,operations}.ts; BC/ChannelMembersDialog.tsx | DONE (ui/members.rs) | — | S |
| Host without v1: no agent controller, harness detection, installers, model lookups, imports, teams, snapshots or logs; keeps Agents custody, declared processes and terminals; no events (#859) | src-tauri/src/{agents,agent_models,harness_setup,managed_npm,goose_models,pi_models,codex_models}.rs | DONE (crates/host/src/lib.rs, api.rs; crates/buzzd) | v1 requests are answered as unsupported and old hosts' snapshot events skipped (host/tests/v1_peers.rs) | L |
| Mission control on Agents: the owner's agents by the host that holds each key; runner state Working / Ready / Elsewhere / Connecting; failed runs and connection stalls need the owner; no Start/Stop; View profile (#859) | — (this port's addition) | DIFFERENT (deliberate: Mission control; app/src/mission_control/{mod,model}.rs, ui/mission_control/{page,row}.rs, agents2/service.rs `run_state`) | Activity frames, where an agent sends them, still drive what it is doing | M |
| Agents2 renamed Agents: page title and label "Agents", key `buzz.agents/agents`, Robot icon in the channel sidebar; "Agents runs in the desktop app" (#871 `ea48cc8d`) | src/bundled/agents/*; src/app/{navigation,shell/AppShell,shell/presentation}.ts | DONE (ui/agents2/mod.rs, ui/agents2/page.rs, ui/pages.rs `Page::AGENTS`) | Rust modules keep the agents2 name | S |
| Plugin switch buzz.agents2 moves to buzz.agents once; the v1 switch is dropped (#871) | crates/plugin-manager migrate_agent_flag.rs; src/plugins/migrate-agent-flag.ts | DONE (buzzodz-plugins at the pin; plugins.rs test) | — | S |
| Storage moves: the host's `agents2` folder to `agents` (refused while another copy runs or when both exist: "Close other Buzz copies, then reopen Buzz to migrate agent storage. Nothing was moved.", "Both agents and agents2 storage exist…"); settings `buzz.agents2.v1` to `buzz.agents.v1`, both left alone when they differ (#871) | src-tauri/src/sign_out/migrate_agents.rs; FA/migrate-storage.ts | DONE (host/lib.rs `migrate_agents` at host start and in buzzd; sign_out.rs `migrate_agents` under the instance lock; agents2/store.rs `migrate_storage`) | The port's folders are `host/agents2` → `host/agents` and buzzd's `<state>/agents2` → `<state>/agents` | S |
| A brought-over agent's type adopts its instructions and workspace (Claude Code both; Codex a valid workspace); Bring over copy says so (#871) | BA/agents/AgentsPage.tsx; FA/service.ts `adopt`; src/bundled/{claude-code,codex}/index.ts | DONE (agents2/registry.rs `adopts`/`adopt`, agent_plugins/{claude_code,codex}/mod.rs, ui/agents2/bring_over.rs) | — | S |
| A failed turn's reply tags the agent's owner, not whoever asked (#871) | src/bundled/claude-code/runtime.ts `report`; src/bundled/codex/runtime.ts `publish` | DONE (agent_plugins/mod.rs `reply_tags`) | — | S |
| Folded audience row keeps its button in the Tab order (#871) | src/features/messages/MessageRow.tsx | N/A: the port has no folded audience row | — | — |
| Re-sign stale agent removals instead of resending them (#912 `dd360ca3`) | FA/relay-removal.ts | DONE (already: agents2/cleanup.rs signs a fresh removal on every attempt and keeps none to resend) | — | — |
| Retry reloads the agent list: members Refresh reads it again and "Some agents could not load" follows the list's own failure; a successful read clears it (#921 `1e1ad0b8`) | src/app/services.ts; FA/{choices,local,service}.ts | DONE (ui/members.rs, agents2/service.rs `refresh`) | No composer "Retry agents" in the port | S |
| Me as project chats: "New chat", a Projects heading, project rows with their disclosure caret and indented chats, "Chats", "Project settings…", "Delete project", "New project…", "Copy chat name/ID", "Chat settings…", "No chats in this project.", "Saving project…", "Create new project"/"Project name", "<name> chats" defaults template, Share's provider-session note. A chat's transcript and composer centered at 52rem with the viewer's messages on the right; under the composer the follow-up assistant ("Choose follow-up assistant", "No assistant"), saved per chat on this device, that plain sends go to (explicit mentions override; "The selected assistant is no longer available. Choose another assistant and retry."), and the project control ("No project", "Retry projects"); a new chat chooses both before it starts ("New chat in <project>"); Me applies no remembered-agent follow-up (#1005) | src/bundled/sessions/{SessionSections,SessionsPage,SessionShare}.tsx; src/features/sessions/{NewSessionComposer,SessionAgentControl,SessionProjectControl,AgentChoice,WorkspaceSettings,selected-assistant}.ts(x); src/features/messages/{MessageComposer,ChannelTimeline,MessageRow,ComposerFormattingTools}.tsx | DONE (app/src/ui/me/{controls,sections,new_session,board,dialogs,settings,workspace,share,recipients}.rs; ui/channel.rs `measure`, `set_selected_agent`; ui/composer.rs `set_chat`, `set_trailing`; ui/message.rs `Rows::align_viewer`; ui/overlay.rs `dropup`) | DIFFERENT: the assistant picker is a menu of the viewer's agents, not the searchable AgentChoice popover with presence; no model/effort controls (the port has no ComposerModels); the viewer's messages cap at 85% of the row, not also 42rem | L |
Agent runtime (stream B1a). `CC/` = `src/bundled/claude-code/`, `CX/` =
`src/bundled/codex/`, `MCP/` = `src/buzz-mcp/`, `AG/` = `src/features/agents`
(`agents2` before #871); `codex/` = `app/src/agent_plugins/codex/`, `claude/` =
`app/src/agent_plugins/claude_code/`.

|---|---|---|---|---|
| Keep agents running across communities, as merged (#838 `02fbc69d`): the merged PR also folded in one `codex app-server` for every agent; a stopped agent's turns interrupt themselves, its leftover shells are cleaned, and it starts again only after letting go; the server reopens after an exit | AG/service.ts; CX/{index,runtime}.ts; CC/{claude,runtime,sessions}.ts | DONE (codex/runtime.rs `Server`, `stop`, `open`; the rest was ported ahead of merge, see 30-C) | The shared server is per host: agents on another buzzd host share that host's | M |
| Model and effort for a mentioned agent's next run (#856 `f9a5e6f5`): `model`/`effort` tags for mentioned agents only; owner-only, type-listed picks reach `Delivery`; Claude lists models from `initialize`, switches with `set_model` / `apply_flag_settings` and restores its own; a short-lived process reads defaults without the shared spare; Codex previews models and runs the picked model at a supported effort; composer gauge button, popover, model menu and effort slider | AG/{service,react}.ts; CC/{claude,sessions,runtime}.ts; CX/runtime.ts; src/features/messages/ComposerModels.tsx; MessageComposer.tsx; src/features/relay/messages.ts | DONE (relay/src/messages.rs `RunPick`; agents2/{registry,service}.rs `models`, `asked`, `picked`; claude/{claude,sessions,runtime}.rs; codex/runtime.rs `preview`; ui/composer/models.rs; ui/composer.rs; ui/src/components/slider.rs) | The slider does not animate | L |
| Show agents working (#860 `ecfbb8a5`): a run counts as working until the work it handed off settles; 👀 and 💬 reactions on the mention, taken back after; the agent typing in the mention's thread on this device; Codex settles on its final send | AG/{service,working-reactions,local-typing}.ts; src/features/relay/typing.ts; CC/runtime.ts; CX/runtime.ts | DONE (agents2/working.rs, agents2/service.rs `working`; the reactions and local typing gone with #484 and #998; claude/runtime.rs; codex/runtime.rs `Answered`) | — | M |
| Agents talk to each other (#865 `7400d680`): a sibling (owner's `auth` tag) wakes an agent only by naming it; Claude and Codex answer the owner and agents it attested (Claude's Answers setting removed); `send` needs an `audience`, `agents` tagged; flat replies; shared Buzz prompt; quiet one-line rows for agent coordination, grouping by audience, no notifications | CC/{base_prompt.md,prompt,runtime,tabs}.ts; CX/{prompt,runtime}.ts; MCP/{prompt.md,tools}.ts; AG/service.ts; src/features/messages/{MessageRow,audience-address,message-grouping}.ts; src/features/notifications/messages.ts; src/features/relay/{audience,fold,incoming,row-identity,session}.ts | DONE (agent_plugins/mod.rs `from_owner`; buzz_mcp/{prompt.md,tools.rs}; agents2/service.rs `heard`; relay/src/audience.rs, relay/src/fold; notifications.rs; ui/message.rs `quiet`; ui/audience_address.rs) | GPUI cannot tell which element was clicked: in an open line, clicks on the body's own controls do not fold it | M |
| No deadline on an agent's work (#874 `81f8de7f`) | CX/runtime.ts; AG/service.ts | DONE (codex/runtime.rs; agents2/working.rs) | — | S |
| `media_get` lets agents see Buzz images (#840 `736975e2`): the host fetches the agent's community media as the agent (Blossom get auth, own community only, 3.75 MiB, hash checked); MCP image content for Claude, input image for Codex; Codex bindings move to `v4`; process writes over 1 MiB go in pieces | MCP/{tools,rpc,app-client}.ts; CX/{rpc,runtime}.ts; ST/app_agents.rs `app_agent_media`; crates/agent-controller/src/app_agent.rs; src/features/host/service.ts | DONE (host/app_agents/media.rs, host `AppAgentRequest::Media` (new variant; older hosts answer unsupported); agents2/runtime.rs `media`; buzz_mcp/{tools,rpc,app_client}.rs; codex/{rpc,runtime}.rs; agent_plugins/process.rs) | Signs with the host's own code until the fork pin carries the controller's `media_auth` | M |
| Wake on mentions only, not unmentioned thread replies (#910 `94fdec47`), and the Attention text | AG/{attention,AttentionPanel,service}.ts | DONE (agents2/attention.rs `addressed_to`; ui/agents2/attention.rs) | — | S |
| Call the agent type "Claude", not "Claude Code" (#924 `350f7d71`) | CC/*; crates/plugin-manager (manifest name) | DONE (claude/*, agent_plugins/process.rs) | The plugin's name in Settings → Plugins comes from the fork pin's manifest, so it reads "Claude" after the pin bump | S |
| The agent's name in the Claude system prompt; a rename restarts idle sessions (#903 `6730a3d2`, prompt/runtime part) | CC/{prompt,runtime}.ts | DONE (claude/{prompt,runtime}.rs, claude/mod.rs) | — | S |
| Session viewer scripts (#903 `6730a3d2` rest, #855 `ca078585`) | scripts/session-*.mjs; docs/session-debugging.md | N/A: buzz-app developer tooling | — | — |
| Codex: a rejected steering mention stops counting as working at once (#919 `c3f6068e`) | CX/runtime.ts | DONE (codex/runtime.rs) | — | S |
| Verified relay workflow mentions (#970 `ef824638`): a community-signed workflow message targeting the agent with `buzz:workflow-mention`, owned by the owner or an agent the owner attested in its signed profile | AG/service.ts `fromOwner`; src/features/relay/workflow-attribution.ts; CC/runtime.ts; CX/runtime.ts | DONE (agent_plugins/mod.rs `workflow_mention_owner`, `from_owner_or_workflow`; owner_relay.rs `authority`, `profile_owner`) | The creator's profile is read over HTTP, not through a profile directory cache | S |
| Codex: a differing model/effort pick runs as the next turn; later follow-ups queue behind it (#889 `d6bc96e0`) | CX/runtime.ts | DONE (codex/runtime.rs) | — | S |
| Codex: log a failed idle terminal clean; none once the server closed (#878 `b351b880`) | CX/runtime.ts | DONE (codex/runtime.rs `stop`) | — | S |
| Restore model and effort picks when a background send is recovered (#887 `d01e2ff5`) | src/features/messages/MessageComposer.tsx | DONE (ui/composer.rs, ui/composer/attachments.rs) | A recovery still replaces unsaved typing, as before, rather than raising a conflict | S |
| Codex gets the Buzz base prompt (#984 `f208fa61`) | CX/{base_prompt.md,prompt.ts} | DONE (codex/base_prompt.md, codex/prompt.rs) | — | S |
| Show agent progress across channels, threads and messages (#484 `f93c3378`): one local lifecycle per mention (received, starting, working, finished, failed; 512 rows kept; the newest handling of a message wins) with the harness's own steps in words ("Reading notes.md", "Running mktemp, xargs", "Received · waiting to start", "Starting", "Continued on a later message"); Claude acknowledges each message by the uuid it echoes (`--replay-user-messages`) and an accepted Codex steer moves its thread's activity to the newer mention; a full queue fails the mention ("Agent is busy and could not queue this request. Try again when it finishes."); Codex failures name why ("Codex was stopped", "Steering was not accepted: …", "Codex’s turn was interrupted", "Codex ended without a final reply"); the 👀/💬 working reactions are removed; a message's activity bubble beside its reactions (avatars, breathing or stepping dots, "Could not finish", "Status unknown"; a click opens the thread even before a reply; resting on it lists each agent's step with a sheen, opens the agent's profile on its activity, and dismisses a failure on this device); thread summaries show the working agents' avatars with waiting/working dots, leaving out a root's own bubble; the sidebar badge breathes while every agent is still starting; "Working now" rows show the local line and no View activity; the composer's typing line leaves out agents a bubble shows; telemetry turns tie to messages by their trigger ids; "View activity" in an agent message's menu and Report under a separator; the profile's Activity tab (`buzz:profile?agent=…&tab=activity`); message avatars lose the thinking badge | AG/{local-activity,request-activity,request-work,thread-working,activity,current-activity,service}.ts; src/bundled/agent-activity/*; src/bundled/channels/{working-agents,useWorkingAgents,WorkingAgentsBadge,ChannelActivityPopover}.ts(x); src/bundled/profiles/*; src/features/messages/{MessageRow,ReplySummary,TypingIndicator}.tsx; src/features/conversation/typing-presentation.tsx; CC/{claude,runtime,sessions}.ts; CX/runtime.ts; design-system TypingDots, ShimmerText, AvatarStack, `buzz-arrive` | DONE (agent_activity/{local,request}.rs; agent_activity/{mod,turns,current,observer}.rs; agents2/{service,working}.rs; agent_plugins/mod.rs `activity`; claude/{claude,sessions,runtime}.rs `Receipt`, `step_label`; codex/runtime.rs `codex_step`, `programs`; ui/agent_activity/bubble.rs; ui/message.rs; ui/message_actions.rs; ui/channel.rs `thread_work`; ui/thread.rs; ui/typing.rs `replace`; ui/sidebar/rows.rs; ui/channel_activity.rs; ui/profile_panel.rs; ui/agent_activity/profile.rs; relay session `retained_events`; ui/src/components/activity.rs). The thread panel's resize anchoring and the WebKit video dimension guard are N/A (DOM layout) | DIFFERENT: the step list is a hoverable tooltip at the pointer, not a popover above the bubble; the bubble fades in and goes at once, without opening and closing its space; an Agents agent's profile opens on Info, where this port keeps its type's sessions; telemetry triggers whose message arrives later are tied within a second instead of on arrival | L |
| An agent reads and writes its own attention (#806 `ddc01962`): one-object shows with Janet's expected-state tokens (`object-v1:` SHA-256, `object-v1:absent`), `conflict` / `wrong-scope` / `invalid-operation` / `not-found` / `invalid` refusals, a watch needs its Interest and never moves or changes type, an Interest goes only after its watches, agent timers at least 60 s apart, 256 KB per agent, unreadable objects left to the owner; the owner's Attention switch ("Lets the agent add and change its own Interests, watches and timers, and wakes it for them…"); watches hear only channels the agent is a member of; queued watch runs re-matched before they run (gone after attention off, a disable, removal or change); timers keep their budget across edits, only a rearm moves the deadline, "Run again" restarts; classifier questions need only `question` (threshold 0.2 by default); classified watches pass with the reason "no classifier is available on this device"; a timer turn says when it is the last | AG/{attention-objects,attention,store,service}.ts; AG/AttentionPanel.tsx | DONE (agents2/objects.rs, agents2/store.rs `Schedule`, `attention_enabled`, agents2/attention.rs, agents2/runtime.rs `AgentAttention`, `ClassifierSkip`, agents2/service.rs `rematched`, `member`, agents2/service/attention_calls.rs, ui/agents2/attention.rs `attention_switch`) | The handle's calls run on the Agents service's thread one at a time, so a write needs no retry loop against a racing one; a queued watch's object is compared by value and modified time, not identity | M |
| Attention tools for an agent to manage its own attention (#807 `ebd529e8`): `interest_set` / `_show` / `_list` / `_remove`, `watch_add` (never replaces; timers run once by default), `watch_update` (only the fields given; `clear`), `watch_enable` / `_disable` / `_remove`, `watch_rearm` (a new deadline from `at`, never in the past; keeps usage, so a spent timer needs higher limits), `watch_show`, `watch_list`; listed only while the session is offered them, refused once the owner turns attention off; every answer names `classifier: available/unavailable` and a classified watch carries the "pass unchecked" note | MCP/{attention,rpc}.ts; AG/attention-objects.ts | DONE (buzz_mcp/attention.rs, buzz_mcp/rpc.rs `AttentionTools`, `respond_with`; agents2/objects.rs `WriteOptions::rearm`) | Answers list their keys in sorted order, not insertion order | M |
| Claude wakes for its watches and timers in one-off turns (#809 `017ec30c`): each wake is a new session that is never saved, in its Interest's lane (two Interests at once, 8 waiting per Interest and 32 in all, the longest-waiting first), with a 5-minute deadline, killed and settled only once its process exits; asked whether it is still current when its lane starts it and again once it has a process, its prompt built then from the attention as it is (watch: Janet's `<watch-task>` framing, `<interest id>` or the missing-Interest note, `<watch-classifier>`, the event as escaped JSON in `<observed-message>`; timer: `spent: true` on its last occurrence); `send` has no default destination there; attention on adds the tools, the `## Attention` guidance and the end-of-turn `<attention>` review, and turning it off drops waiting wakes and restarts idle sessions; a watch hears anyone, a mention still needs the owner | CC/{attention_prompt.md,lanes,prompt,runtime,sessions}.ts; AG/service.ts | DONE (claude/{attention_prompt.md,lanes.rs,prompt.rs,runtime.rs,sessions.rs `run_once`}, claude/mod.rs, agents2/runtime.rs `Current`, agents2/service.rs `still_current`, buzz_mcp/app_client.rs `AgentActions::attention`) | The Answers help text change is N/A: #865 removed that setting | L |
| Attention stays off until the owner turns it on (#951 `76c1d9f1`): only "on" is saved (`attentionEnabled: true`); a new or existing agent, or one saved with the old "off", reads as off, so its watches and timers wake nothing and it gets no attention tools, guidance or review | AG/{service,store}.ts | DONE (agents2/store.rs `attention_on`, agents2/service.rs `save`, `update`) | — | S |
| Session transcripts in the Claude and Codex tabs (#842 `fc8d324f`): "Conversations" lists the 30 most recently used saved sessions (`#name` or channel id · thread or "channel"; Working/Idle or Codex status; date) with "View transcript"; the transcript reads on open and on Refresh, with Back, "Reading the … session…", the missing and "Could not read the … session: …" copy and "Showing the latest N turns."; Claude reads `~/.claude/projects/<project>/<session>.jsonl` (50 turns; steering folded in, sidechain and meta skipped), Codex `thread/turns/list` from a short-lived app-server (20 turns); Codex bindings record `at` and move to the end when saved | CC/{transcript,tabs,runtime,sessions}.ts; CX/{transcript,tabs,runtime}.ts; AG/ActivityTranscript.tsx; MCP/app-client.ts | DONE (claude/transcript.rs, codex/transcript.rs, agent_plugins/transcript_view.rs, claude/{tabs,runtime,sessions}.rs, codex/{tabs,runtime}.rs, ui/agent_activity/transcript.rs `TranscriptHost`; host/processes.rs declares Claude's `workspace` (`pwd`)) | One shared transcript view instead of upstream's two near-copies. A host from before the `workspace` process says to update it | M |
| The clicked conversation's session set apart in the Claude and Codex tabs (#846 `d23a46e1`): a profile opened from a message (avatar, workflow owner, mention) carries the message's channel and thread root (`threadRootId ?? id`) to the type's tab; a "This thread" / "This channel" card above "Recent conversations" (30) shows the thread's session, else the channel's, with "View transcript", "Last active …", "Looking for a … session…", "Could not look for a … session: …", "No … session here yet" / "One starts when you mention the agent in this …"; reopening the profile from another conversation retargets its open panel and closes a shown transcript; transcripts open over the tab with "Back to profile" (Escape too) and Refresh. Sessions other copies of the app started are found: Claude greps the project folder (new declared `find`) and keeps files whose first prompt names the same channel, scope, thread and agent (turn prompts gain `Agent: <pubkey>`); Codex thread names end in `buzz:` + FNV-1a of agent, channel and root, found with `thread/list` by that tag or the earlier readable name (checked against the turns' context). Transcripts merge all the conversation's sessions oldest first; Codex reads one turn per request; what the agent sent shows as its reply marked "· Posted" (only once sent; a failed Claude send adds "The message above was not posted: …"); Codex commands without the login-shell wrapper, empty final answers left out, prompts name their sender; Claude's MCP tools named `buzz.read` | AG/{OpenedConversation,ActivityTranscript,activity-transcript,service,tabs}.ts(x); CC/{tabs,transcript,runtime,prompt}.ts(x), manifest.json; CX/{tabs,transcript,runtime,prompt}.ts(x); BP/ProfilePanel.tsx; src/features/messages/{MessageRow,ThreadPanel,ChannelTimeline}.tsx; src/bundled/channels/{ChannelsPage,ChannelBody,ConversationTab,useChannelTabState,useConversationTabs}.ts(x); src/features/panels/service.ts | DONE (agent_plugins/opened.rs, agent_plugins/transcript_view.rs, claude/{tabs,transcript,runtime,prompt}.rs, codex/{tabs,transcript,runtime,prompt}.rs, agents2/registry.rs `AgentConversation`, ui/agents2/{tabs,mod}.rs, ui/profile_panel.rs, ui/navigation.rs `LinkOrigin`/`show_profile_from`, ui/message.rs, ui/markdown.rs, ui/channel_tabs.rs `open_panel`, ui/channel_panels.rs `Opening::root`, ui/channel_tab_views.rs, ui/workspace{,/board}.rs, ui/me/board.rs, ui/agent_activity/transcript.rs; host/processes.rs declares Claude's `find` (`grep -rlF --include=*.jsonl -e`)) | DIFFERENT in part: the transcript covers its tab rather than opening as a local detail tab beside the profile, and a retargeted profile panel is rebuilt rather than updated in place. A host from before the `find` process says to update it | L |

Agents page and agent creation (stream A2). `BA/` = `src/bundled/agents/`,
`page` = `app/src/ui/agents2/`.

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Teams (#867 `146caad4`): a team is a name, instructions and agent members kept on this device (`buzz.agents.teams.v1`); deleting a team leaves its agents, a deleted agent leaves every team; "Name the team", "A member is not an agent on this device", "No such team" | FA/{store,service}.ts `readTeams`, `saveTeam`, `removeTeam` | DONE (agents2/teams.rs, agents2/store.rs `Team`, agents2/service.rs `teams`, `save_team`, `remove_team`, `prune`) | — | S |
| Each run gets a `<team>` section per team the agent is on (roster by name and npub, "you", `<team-instructions>`); Claude puts it after `<agent-instructions>` and restarts idle sessions when it changes; Codex sends it with the per-turn instructions and "Current team sections replace all prior team sections. None means you are on no team." (#867) | FA/service.ts `teamPrompt`; CC/{prompt,runtime}.ts; CX/{prompt,runtime}.ts | DONE (agents2/teams.rs `team_prompt`, runtime.rs `Delivery::team`; claude/{prompt,runtime}.rs; codex/{prompt,runtime}.rs) | — | S |
| Teams side panel from More → Teams: list with members or "No members", "No teams yet.", "New team"; editor with Name, Instructions ("Every member follows these after its own instructions."), Members checkboxes ("No agents yet."), "Create team"/"Save", "Delete team"; Escape returns (#867) | BA/AgentsPage.tsx `Teams`, `TeamEditor` | DONE (page/teams.rs, page/page.rs `open_teams`) | — | M |
| The owner's agent list (#870 `287a557d`): every agent here is listed in an owner-signed kind 30177 record with `["runtime", "<plugin>/<type>"]`; a record keeps every setting and tag of the one it replaces, defaulting `parallelism` 1 and `respond_to` "owner-only"; read once per community connect, retried on the next sync, writes made during a read survive it; Delete removes the record after the archive | FA/{service,relay-removal}.ts `publishAgentRecord`, `deleteAgentRecord`, `listingsOf` | DONE (agents2/listings.rs, agents2/service/listed.rs, agents2/cleanup.rs; identity signs kind 30177 as content) | Records are read and written over HTTP (`/query`, `/events`), not through the outbox | M |
| Agents another device runs show as "On another device", read-only, never run here, no Delete; peek "Its key is on another of your devices, which runs it. Open Agents there to change or delete it."; records without a runtime stay hidden (#870) | BA/AgentsPage.tsx; FA/service.ts `listedAgents` | DONE (agents2/service/listed.rs `elsewhere`, page/page.rs `Shown::Elsewhere`) | An agent on a buzzd host that is offline still counts as here | S |
| Hosted agent types, Setup unfinished / Finish setup, provider-run delete (#870) | FA/service.ts `remote` | SKIPPED (decided: Builderlab) | — | — |
| Agents page tabs Your agents / Import / Browse in a centred 48rem column; header "Agents" with the robot and a More menu (#913 `0cf13d71`) | BA/AgentsPage.tsx, AgentsPage.module.css | DONE (page/page.rs) | — | M |
| Card grid (3 columns, 2 under 560px, 1 under 360px), 15.25rem cards with a 6rem portrait (8rem for a pack avatar), name and type mark plus summary; New agent tile with its pack-avatar scatter; listbox keys as before (#913) | BA/{AgentCard,NewAgentTile}.tsx, AgentCard.module.css | DONE (page/card.rs, page/page.rs) | GPUI cannot rotate the tile's avatars, so they sit upright | M |
| Create agent modal: random pack avatar, Choose avatar → Fuzzies / Gloopies / Figgies / Use image (avatar editor) → a pack's avatars sized to fill the card; name; Claude's and Codex's instructions ("Add instructions"/"Edit instructions", examples rotating every 5 s, an editor over the card with Back and "Done editing"); footer type menu with each type's model submenu (default row, models, "Custom model" form "Enter a model ID"/"Use model"); "Create agent"; a started draft closes only from Close; a picture that fails to save does not fail the create (#913, #952 footer) | BA/{AgentCreateDialog,AgentCreateHeader,PackedAgentAvatar}.tsx; FA/{AgentInstructions,ModelPicker,avatar-packs,create-card}.ts(x) | DONE (page/create.rs, page/packs.rs, agents2/registry.rs `create_instructions`, `create_models`, `with_model`, `create_model_label`, agents2/service.rs `create` with `Made`) | DIFFERENT: the pack PNG posters and catalog are bundled but the CDN animations are not played; the type sections are built in for Claude and Codex (plugins are being redone); the Host select stays in the footer (this port's own) | L |
| Import tab replaces Bring over: cards with source label, key label only for a repeated name, npub and community in a tooltip, "Import", "Import as", "No other agents found to import.", "Enable an agent type plugin to import agents." (#913) | BA/AgentsPage.tsx `ImportAgents` | DONE (page/bring_over.rs) | Rows from several hosts still name their host | S |
| Browse tab: shared agents in the community, read-only, paged with Load more, details and Import as a new agent (#913) | BA/AgentBrowse.tsx; FA/{shared-agents,catalog-envelope}.ts | DONE (page/browse.rs, agents2/service.rs `port_shared`) | Envelopes are checked with buzz-desktop's validator at the pin | M |
| Types provide `Create`, `CreateCorner`, `useCreateCard` (#913) | FA/{service,create-card}.ts | N/A: plugins being redone; Claude's and Codex's sections are built in | — | — |
| Soft & wide agent icon (#932 `2ad929e5`) | DS/icons/Robot.tsx | DONE (assets/icons/custom/robot.svg, ui/src/icon.rs) | — | S |
| Hosted Builderlab types and their login (#915, #916, #931, #978) | src/bundled/builderlab/** | SKIPPED (decided: Builderlab) | — | — |
| Read-only Memory tab in Build and the profile panel: core then other memories, Markdown cards, sort by name or newest with relative times, copy name or body, refresh, loading/error/denied states (#982 `33b0e15f`) | FA/{MemoryPanel,memory,tabs}.ts(x); BA/AgentTabs.tsx; src/bundled/profiles/ProfilePanel.tsx | DONE (page/memory.rs, page/tabs.rs) | DIFFERENT in small ways: a session not yet connected says "Memory can be read once Buzz is connected." and loads once ready; no caret animation | M |
| One memory contract in the Buzz prompt; Claude's duplicate guidance removed; a shared core-memory section (#982) | MCP/prompt.md; CC/{base_prompt.md,prompt.ts} | DONE (agent_plugins/base_prompt.md holds the contract since #1008; agent_plugins/harness.rs `core_memory_section`; claude/prompt.rs) | — | S |
| Codex gets current core memory every turn, goes on without it, does not call a partial listing missing, shares reads and drops them after a write (#982) | CX/{runtime,prompt}.ts, base_prompt.md | DONE (codex/{runtime,prompt}.rs; agent_plugins/harness.rs `MemoryReads`, `core_of`; agent_plugins/owner_relay.rs `memory_listing`) | — | S |
| Agents → More → Provider settings (#952 `7503384d`): a row per type with its mark, a spinner while checking, the attention label ("Install Claude Code", "Codex needs attention") or "<type>, unavailable"; opening or hovering the submenu checks every provider; a row opens the provider's dialog (the type's mark and title; closes on an outside press unless setup is running) | BA/AgentsPage.tsx `ProviderSubmenu`, `ProviderDialog`; FA/service.ts `AgentProvider` | DONE (agents2/registry.rs `AgentProvider`, `ProviderChanges`; page/page.rs; agent_plugins/provider.rs) | — | M |
| Provider readiness and recovery on this computer: Claude's install/sign-in/check and Codex's account and models, checked on the embedded host; device defaults (`buzz.claude-code.provider-defaults.v1` model; Codex model and reasoning) with "Default model", "Save defaults", catalog loading and retry; a blank agent model inherits the device default at run time; "Provider default · <model>" rows in New agent and Settings (#952) | CC/{defaults,setup,tabs}.ts(x); CX/{index,setup,tabs}.ts(x) | DONE (claude/{provider,defaults}.rs, codex/{provider,defaults}.rs, claude/runtime.rs, codex/runtime.rs, claude/tabs.rs, codex/tabs.rs) | The install row follows this port's installer until the managed-install sync lands | L |
| Provider dialogs are routes: `{"provider": "<plugin>/<type>"}` opens the dialog (pushed), closing replaces it with the page start (#952) | BA/{routes,index}.ts(x) | DONE (page/routes.rs, page/mod.rs `register_routes`, page/page.rs) | — | S |
| Port-only `{"agent": "<pubkey>"}` route opens an agent's Build view; Mission control's row menu "Open agent" uses it instead of View profile | — (this port's Mission control) | DIFFERENT (deliberate: mission control; page/routes.rs, ui/mission_control/row.rs) | — | S |
| Change an agent's provider in Settings (#952): a Provider select at the top of the Claude and Codex Settings tabs; "Move <name> to <type>?" previews what carries over (instructions, a valid workspace) and what resets or is dropped, with the old type's warning; Continue stages the destination's settings; Save commits provider and config together; refused while the agent is answering; works when the old type is unavailable; Build's rail no longer shows Type | FA/{service,native,tabs}.ts(x) `previewRetype`, `retype`; BA/{AgentSettingsHost,AgentTabs}.tsx; crates/agent-controller app_agent.rs `set_type`; ST/app_agents.rs | DONE (agents2/registry.rs `retype_plan`, `carry`, `describe_migration`; agents2/service.rs `preview_retype`, `retype`; agent_plugins/tabs.rs, claude/tabs.rs, codex/tabs.rs, page/tabs.rs, page/page.rs; host `AppAgentRequest::SetType` (new variant; older hosts answer unsupported: "Update the agent's host to change its provider.")) | Claude's and Codex's status and sessions stay separate tabs rather than Settings' "Runtime diagnostics" | L |
| Settings controls at parity with New agent: model picker with the model's mark and a "Custom model" entry, Instructions field "Describe what I should do…" (#913, #952) | FA/{ModelPicker,AgentInstructions}.tsx; CC/tabs.tsx; CX/tabs.tsx | DONE (agent_plugins/tabs.rs, claude/tabs.rs, codex/tabs.rs) | — | M |
| Codex accepts `~/…` workspaces, expanded by the host before launch (#952) | CX/{config,runtime}.ts | DONE (codex/config.rs, codex/runtime.rs) | — | S |
| Select/Combobox/RadioGroup/Accordion design-system changes and browser permission tests (#952) | DS/ui/*; src-tauri capabilities | N/A: web components and Tauri permissions | — | — |
| Relay owner-view memory read queue (#982) | FA/memory.ts `read` | N/A: memory reads are one HTTP request each; there is no view pool to protect | — | — |
| Buzz's own copies of Claude and Codex, kept on the latest release (#949 `75c501b3`): `claude.sh`/`codex.sh` run/install/use steps (checksum, smoke test, `current` switched under `flock`, older releases pruned, a newer one kept); the launcher's 127 reads as "not installed"; Claude older than 2.1.121 is "older than Buzz needs" with "Update Claude"; updates on load, when a type's first agent runs and every 6 hours, the first download only once an agent runs or on Install; a release that fails its test is skipped until asked and named in a notice ("… failed its test: …. Buzz keeps using …"); a mention waits for the first download within its deadline and retries a failed one; `DISABLE_AUTOUPDATER`; Codex tab and provider dialog: "Install Codex" / "Sign in", "Installing Codex…" / "Waiting for Codex sign-in…" with Cancel, setup output, install copy | CC/{claude.sh,setup.ts,index.ts,tabs.tsx,claude.ts}; CX/{codex.sh,setup.ts,index.ts,tabs.tsx} | DONE (app/agent_plugins/managed.rs shares the setup both plugins mirror; claude/setup.rs, codex/setup.rs, claude/provider.rs (the shared setup controls and the provider's "Update Claude Code"), codex/provider.rs ("Install Codex", blocking while a step runs), codex/tabs.rs; host/processes.rs with host/processes/{claude,codex}.sh) | DIFFERENT details: the host pins the scripts and declares each step (`claude`, `codex`, `app-server`, `install`, `use`, `platform`) instead of granting `bash -c` to whatever the GUI sends; each host keeps its own copy, updated when that host's first agent runs; a remote host signs Codex in with `--device-auth`; the GUI fetches the release listings; NixOS's system profile joins the processes' `PATH` | M |
| Continue Me chats as one top-level conversation (#1000 `edf8ee58`): a mention counts as a Me chat only when the owner is the viewer, the channel is no DM and the owner's Me placement holds it (refreshed once; unreadable fails the turn: "Couldn't read your Me chats to tell where to answer (…). Try again."); Me turns use the channel's session and history, `posting: top-level` / the top-level instruction, tools refuse a reply into the chat ("This chat stays at the top level: send again without reply."), failure notices carry no `e` tag; a request under the other policy waits for the conversation's turns instead of steering; base prompts and Claude's session model name the exception | MCP/{client,prompt,tools}.ts; AG/me-chat.ts; CC/{runtime,prompt}.ts, base_prompt.md, session_model_thread.md; CX/{runtime,prompt}.ts, base_prompt.md | DONE (agent_plugins/owner_relay.rs `me_chat`; claude/runtime.rs `Policy`, `key`; claude/prompt.rs; codex/runtime.rs, codex/prompt.rs; buzz_mcp/{mod,tools}.rs) | Typing for a Me chat is in the channel itself with #998's pulses; the activity-bubble scope belongs to the activity panel (#484) | M |
| One Buzz base prompt for Claude and Codex (#1008 `45e8b536`): the shared text (Codex now reads that silence is fine and that its `core` memory is injected), one owner read of an agent's memory per community at a time that turns and tools share and a write forgets, the `<core-memory>` section; Claude reads `core` afresh for each new session (no five-minute reuse); a Codex turn that ends without a final send just ends, with no "ended this turn without a final reply" notice | src/features/agent-harness/{base_prompt.md,base-prompt.ts,core-memory.ts}; CC/{prompt,runtime}.ts; CX/{prompt,runtime}.ts; MCP/prompt.{md,ts} | DONE (agent_plugins/harness.rs, agent_plugins/base_prompt.md with the Agent Memory section moved from buzz_mcp/prompt.md; claude/{prompt,runtime}.rs; codex/{prompt,runtime}.rs) | — | S |
| Agents show as typing to everyone while they work on a mention (#998 `979a2907`): agent-signed kind 20002 every 3 s through the agent's own event route, in the mention's thread (`h`, root `e` when there is one, reply `e`) or in the channel itself for a Me chat, placed after the same Me read as the turn and not at all when that fails; from the run's first hand-off; turns in one conversation share one pulse until the last stops; a slow pulse holds the next; one failure is ridden out, three in a row stop it, as do the turn's end and 30 minutes; pulses are not counted as the agent's own writes; the local-only typing and the relay typing store's local case are gone; agent requests share one HTTP client | AG/{typing,service}.ts; src/features/relay/typing.ts; crates/agent-controller app_agent.rs `KINDS`; src-tauri/src/app_agents.rs | DONE (agents2/typing.rs, agents2/working.rs `on_first_hold`, agents2/service.rs `working`, agents2/runtime.rs `signal`; relay/typing.rs, relay/session/mod.rs; host/app_agents.rs, host/app_agents/media.rs `client`) | 20002 was already allowed by the fork pin's `KINDS`; the 👀💬 working reactions are gone with #484 | S |
| Say the agents this copy runs are online (#862 `dfbb239b`): owner presence, kind 20003 signed by the viewer (`online`/`offline`, one `p` per agent, at most 256 per event), on the live socket only; per community, the agents this copy holds (claim answered) and runs with a ready session; sent on a new session, when the set changes and every 60 s; agents it stops running go offline unless another copy took them over; a failed send is tried again after 30 s, a session that can never send it is not asked again | AG/service.ts `announce`; src/features/relay/session.ts `publishAgentPresence`; src-tauri/src/relay.rs | DONE (agents2/presence.rs, agents2/service.rs `announce`, `runs`, `yielded`; relay/session/{mod,presence}.rs `publish_agent_presence`; identity/lib.rs `AGENT_PRESENCE` signer check) | Readers need no change: the relay folds 20003 into the presence snapshot | S |
| Channel and member management tools (#1055 `80d0161d`): `channels` with `all` (flags forum/dm, private, archived, not joined); `channel` details or create/update/join/leave/archive/unarchive/delete, each refusing fields it does not take, the current channel only when `channel` is left out; `members` add/remove/role (9000/9001) by pubkey or npub, each person once, self redirected to join/leave, not both at once, at most 50, each reported on its own; writes confirmed by reading the channel back (five tries, 400 ms apart); name, about and TTL checks as the app's; only a session's name can change; the base prompt drops its tool list; Codex saved sessions move to `v5` | MCP/{tools,client,app-client}.ts; src/features/agent-harness/base_prompt.md; CX/runtime.ts; crates/agent-controller app_agent.rs `KINDS` | DONE (buzz_mcp/tools.rs, agent_plugins/base_prompt.md, codex/runtime.rs; relay/lifecycle.rs `channel_id`) | `create` with `template` uses the owner's channel templates on this device (#867): found by name first ("No template named …", "N templates are named …"), then the Canvas is started and each of the template's agents here is added, or none when the Canvas could not start (agents2/service.rs `channel_templates`, agents2/runtime.rs `AgentAttention::templates`, buzz_mcp/app_client.rs); the fork pin's signer already allows the channel kinds | M |
| Codex: custom instructions arrive whole (#1059 `15e21f50`): the thread's developer instructions carry only the Buzz guidance; each turn's context says who the agent is, its instructions, the team line and the agent's team sections, and core memory, split into `buzz.instructions`, `buzz.instructions.001`, … of at most 4,000 bytes each, at line breaks where possible, so Codex truncates none | CX/{prompt,runtime}.ts | DONE (codex/prompt.rs `developer_instructions`, `turn_context`; codex/runtime.rs) | — | S |
| Claude: offer every model Claude lists (#1086 `51b8d969`): the Provider settings default-model picker, the New agent menu and the agent Settings tab's Model picker list Claude's own catalog (minus `default`, which is the picker's "Claude default" row) instead of Opus/Sonnet/Haiku; a saved model Claude has not listed stays selectable; the provider dialog says "Loading Claude's models…" and "Couldn't load Claude's models: …" with Try again; device defaults take any model; effort levels come from each model's `supportedEffortLevels` (`max` still dropped), and an unlisted own model offers the efforts listed models have | CC/{tabs.tsx,runtime.ts,setup.ts,defaults.ts} | DONE (claude/provider.rs `model_choices`, `ProviderBody`, `load_models`; claude/defaults.rs; claude/runtime.rs `efforts`) | — | S |
| Separate the agent runner and its reads from the desktop app (#961 `3c2d4cfa`) | src/features/agents/{runner,reads,service}.ts; CC/run.ts; CX/run.ts | N/A (plugins being redone): a plugin-architecture refactor that moves code so a standalone host can reuse it, with no change to what agents do in the app | — | — |

## DONE count

These features match buzz-app closely enough and are not listed above.

| Area | DONE | Items counted |
|---|---|---|
| Page frame and inventory | 10 | Header title; "Add agent" button; page menu "Refresh agents"; "Reading local agent status…"; "statuses may be out of date"; empty-state copy; sort by name then pubkey; tile grid column math; runtime-unavailable alert; live status updates (push events replace polling) |
| Agent card | 8 | Card chrome and min height; 80px squircle portrait with initial fallback; name truncation; "Restart required" badge; "Actions for {name}" trigger; "Manage agent"; "Edit"; "Delete" (danger) |
| Manage dialog | 9 | Title; process label with "Last known:"; "Starts with this app."; error text; profile-pending line + "Retry profile"; Start / "Retry start" (prominent); Stop; relay line; process-status label table |
| Add agent dialog and fields | 35 | Title; description; 860px width; sidebar "Create agent"; Name; Agent instructions (6 rows); "AI configuration" legend; "(install first)" disabled harnesses; Custom executable + Executable; provider inherit labels; Custom provider; "LLM Provider" label; Pi "(API key needed)"; "Loading signed-in providers…"; model placeholders; "Type a model ID…"; "Loading models…"; "Retry models"; Conversation context select + description; Environment accordion (Workspace, Arguments); `BUZZ_ACP_AGENTS=10` pre-listed; env row placeholders and states; Remove/Undo; Variable name + Add variable + both errors; footer helper; five validation messages; "Creating agent…" / "{name} was created. Starting it…"; footer Cancel/Close + Create/Creating…/Start agent; request-id reuse on retry; preset note; saved-key placeholders; Pi model pick sets provider and model; write-only saved env keys; Goose catalog; Pi catalog |
| Edit agent dialog | 7 | Title, width and close X; identity block; "Save changes" disabled until dirty; "Discard changes" when stale; Cancel; Relay detail; Host diagnostics |
| Delete dialog | 1 | Title, body, Cancel, destructive "Delete agent", inline error |
| Import from old Buzz | 2 | Installed/Development source select with reload; stripped-team-instructions note |
| Misc agent features | 1 | Harness preset metadata (Hermes, Claude Code) |
| Agent control (frontend contract) | 5 | Start on app launch; retry profile publication; create (prepare + owner attestation + commit); delete, import preview/commit and save defaults requests; restart action |
| Native backend | 9 | Restore start-on-launch agents at host start; shut agents down on exit; publish pending profiles at start; creation-profile publication with current check; login-shell PATH discovery; owner attestation signed by the GUI identity; import preview/commit in the host; save defaults; isolated model lookups (cleared env, process-group kill) |
| Harness list | 1 | Harness rows with Ready status and "Check again" |
| **Total** | **88** | |

## Counts by status

| Status | Rows |
|---|---|
| DONE | 379 |
| DIFFERENT | 8 |
| MISSING | 0 |
| SKIPPED | 8 |
| N/A | 12 |
| DONE (counted only) | 88 |

Statuses were reconciled with the code on 2026-10-10; section 30 on 2026-10-11.

The rows once marked IN PROGRESS are reconciled with the code: each is DONE now.
