# buzz-app visual spec for the GPUI port

Implementation reference for a one-to-one visual match of Block's buzz-app
(React + Tauri) in Rust/GPUI. Every section cites its buzz-app source files.
Unless noted, paths are relative to `/home/ben/projects/buzz-app/`
(and `DS/` = `src/shared/design-system/`).

Companion files:

- `docs/port/tokens-resolved.json`: every CSS custom property on `:root` of the
  running web shell, dumped by Chromium in light and dark mode. Colors are
  resolved to `rgb()/rgba()`, including `color-mix()`/`oklch()`; non-color
  tokens keep the raw value. Use it to cross-check the tables in section 1.
  Regenerate with `scripts/reference/dump-tokens.mjs`.
- `docs/port/screenshots/`: reference PNGs (see section 9).
- `scripts/reference/`: the capture scripts.

Conventions:

- 1rem = 16px at interface size 100%. The app scales every rem value with the
  "Interface size" setting (0.8-2.0); px values in CSS do not scale (see 1.1, 2.0).
- Light / dark are selected by `<html data-color-mode="light|dark">`. The default
  preference is "system" (`prefers-color-scheme`).
- Where the source and `DS/DESIGN.md` disagree, this spec follows the CSS and
  says so.

## Contents

1. Design tokens
2. Shell layout
3. Channel sidebar
4. Channel page
5. Agents page (5a) and Settings shell (5b)
6. Component visuals
7. Icons
8. Fonts
9. Reference screenshots

## 1. Design tokens

All paths are relative to `/home/ben/projects/buzz-app/src/`. "DS" = `shared/design-system/`.
Every value below is resolved to its final literal for **Light (L)** and **Dark (D)**.
All `rem` values assume a 16px root at 100% interface size (see 1.1 "Interface size" for scaling).
Alpha hex such as `#ffffff85` is also shown as `rgba(...)` with alpha = byte/255.

### 1.1 Theme switching and appearance settings

Sources: `shared/theme/service.ts`, `app/AppearanceSettings.tsx`, `DS/ui/ThemePicker.tsx`,
`DS/styles/settings-controls.css:1-85`, `DS/styles/tokens.css:57,160,399,592,809`,
`shared/styles/tokens.css:4-5,48-49`, `app/startup.css`, `DS/theme/useColorScheme.ts`.

| Aspect | Behaviour |
|---|---|
| Mode selector | `<html data-color-mode="light\|dark">`. All dark token blocks match `:root[data-color-mode="dark"], .dark`. Light is `:root, .light`. |
| Preference | `"system" \| "light" \| "dark"`, stored in `localStorage["buzz-appearance.v1"]`. Default `system`. |
| System mode | `matchMedia("(prefers-color-scheme: dark)")`. Changes are followed live while preference is `system`. |
| Switch transition | During a mode change the app injects `*,*::before,*::after{transition:none !important}` and removes it after 2 animation frames. The palette swap is instant. |
| Native color scheme | `:root { color-scheme: light }`, dark sets `color-scheme: dark` (native scrollbars and form controls follow). |
| Window/theme-color | `<meta name="theme-color">` = `--workspace` = `--surface-base` (L `#f5f5f6`, D `#000000`). The launch screen background uses the same value. In dark mode the launch logo uses `filter: invert(1)`. |
| Scoped theme | `.dark` / `.light` classes re-bind the whole palette on a subtree. `PopoverPopup colorMode="dark"` adds the class to a popup. The DS viewer (not the app) toggles `<html class="dark">` via `useColorScheme` (`localStorage["buzz-design-viewer-color-scheme"]`). |
| Accent themes | **None.** There are no user-selectable accent colors. Primary actions are neutral (black in L, white in D). Purple is a fixed "identity accent". |
| Backdrop choice | `--bg-app = --gradient-1` always (L "Sky field", D "Night garden"). `--gradient-2..4` exist as paired slots (registry `DS/tokens/registry.ts:198-226`), but nothing in the app selects them. |
| Interface size | Second appearance setting: `localStorage["buzz-font-scale.v1"]`, range 0.8–2.0, step 0.1, default 1. Applied as `:root { font-size: calc(100% * var(--buzz-text-scale)) }` (`shared/styles/globals.css:409-413`), so **all rem values (type, spacing, radii, icons) scale together**. UI: `−` / `NN%` / `+` small Buttons, plus a Reset IconButton when the value is not 1. |
| ThemePicker visual | Three radio "thumbnails" (System / Light / Dark). Each is `5rem` wide at aspect 10:7, radius 8px. Outline `1px solid border-standard` with offset 2px. Hover outline = `border-prominent`. Checked outline = `2px solid border-focus`. Keyboard focus outline = 2px `border-focus` with offset 4px. Gap 12px. The SVG preview (80×56 viewBox) draws a backdrop rect (rx 8, `surface-base`), a panel rect (x5 y5 70×46, rx 5, `surface-panel`), a divider at x=23 (`border-standard`), lines with stroke 2 round (`text-metadata`), two accent bars (`affordance-accent-prominent` `#8e4ec6`), and a composer outline (`border-standard`). "System" clips the light preview to the left half and the dark preview to the right half. |

### 1.2 Palette (Layer 0)

Source: `DS/styles/tokens.css:57-261` (Radix Colors values), neutral at `:290-313` / `:407-429`.

**Neutral ramp** (hand-authored, not Radix gray):

| Step | L | D | Buzz use |
|---|---|---|---|
| neutral-1 | `#ffffff` | `#000000` | L panel/popover; D base; text-inverse |
| neutral-quiet-hover | `#f1f1f2` | `#2e2e2e` | subtle control hover |
| neutral-2 | `#f5f5f6` | `#101010` | L base/inset/subtle fill; D inset |
| neutral-3 | `#e8e8e8` | `#1a1a1a` | L border-standard/selected; D panel; disabled fill |
| neutral-raised | — | `#282828` | D popover surface |
| neutral-4 | `#dadada` | `#232323` | L subtle-pressed; D subtle fill, border-standard |
| neutral-5 | `#cccccc` | `#333333` | D selected / floating-row hover |
| neutral-6 | `#b3b3b3` | `#404040` | D floating control / popover selected |
| neutral-7 | `#959595` | `#595959` | text-unavailable |
| neutral-8 | `#808080` | `#737373` | border-prominent; L offline dot |
| neutral-9 | `#5f5f5f` | `#808080` | L text-metadata; D floating-control border |
| neutral-10 | `#525252` | `#a4a4a4` | L text-subtle; D text-metadata, offline dot |
| neutral-11 | `#191919` | `#c7c7c7` | inverse surface; prominent hover; D text-subtle |
| neutral-12 | `#000000` | `#ffffff` | prominent fill, focus border |
| neutral-action-pressed | `#525252` | `#b8b8b8` | prominent pressed |
| neutral-standard-text | `#0f0f0f` | (unused; D uses neutral-12) | L text-standard |
| neutral-navigation-text | `#404040` | (same, unused in D) | L sidenav label |
| neutral-navigation-unread | `#262626` | (same, unused in D) | L sidenav unread |
| neutral-navigation-quiet | `#f7f7f7` | `#232323` | sidenav quiet hover/selected |
| neutral-avatar-on-white | `#525252` | `#525252` | fixed fallback text on white avatar |

**Chromatic ramps** (steps 1→12):

| Hue / mode | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| purple L | fefcfe | fbf7fe | f7edfe | f2e2fc | ead5f9 | e0c4f4 | d1afec | be93e4 | 8e4ec6 | 8347b9 | 8145b5 | 402060 |
| purple D | 18111b | 1e1523 | 301c3b | 3d224e | 48295c | 54346b | 664282 | 8457aa | 8e4ec6 | 975acc | d19dff | ecd9fa |
| red L | fffcfc | fff7f7 | feebec | ffdbdc | ffcdce | fdbdbe | f4a9aa | eb8e90 | e5484d | dc3e42 | ce2c31 | 641723 |
| red D | 191111 | 201314 | 3b1219 | 500f1c | 611623 | 72232d | 8c333a | b0484f | e5484d | ec5d5e | ff9592 | ffd1d9 |
| green L | fbfefc | f4fbf6 | e6f6eb | d6f1df | c4e8d1 | adddc0 | 8eceaa | 5bb98b | 30a46c | 2b9a66 | 218358 | 193b2d |
| green D | 0e1512 | 121b17 | 132d21 | 113b29 | 174933 | 20573e | 28714f | 2f7c57 | 30a46c | 33b074 | 3dd68c | b1f1cb |
| amber L | fefdfb | fefbe9 | fff7c2 | ffee9c | fbe577 | f3d673 | e9c162 | e2a336 | ffc53d | ffba18 | ab6400 | 4f3422 |
| amber D | 16120c | 1d180f | 302008 | 3f2700 | 4d3000 | 5c3d05 | 714f19 | 8f6424 | ffc53d | ffd60a | ffca16 | ffe7b3 |
| blue L | fbfdff | f4faff | e6f4fe | d5efff | c2e5ff | acd8fc | 8ec8f6 | 5eb1ef | 0090ff | 0588f0 | 0b5fa8 | 113264 |
| blue D | 0d1520 | 111927 | 0d2847 | 003362 | 004074 | 104d87 | 205d9e | 2870bd | 0090ff | 3b9eff | 83c4ff | c2e6ff |
| cyan L | fafdfe | f2fafb | def7f9 | caf1f6 | b5e9f0 | 9ddde7 | 7dcedc | 3db9cf | 00a2c7 | 0797b9 | 107d98 | 0d3c48 |
| cyan D | 0b161a | 101b20 | 082c36 | 003848 | 004558 | 045468 | 12677e | 11809c | 00a2c7 | 23afd0 | 4ccce6 | b6ecf7 |
| orange L | fefcfb | fff7ed | ffefd6 | ffdfb5 | ffd19a | ffc182 | f5ae73 | ec9455 | f76b15 | ef5f00 | cc4e00 | 582d1d |
| orange D | 17120e | 1e160f | 331e0b | 462100 | 562800 | 66350c | 7e451d | a35829 | f76b15 | ff801f | ffa057 | ffe0c2 |

Notes: blue-11 L is tuned (`#0b5fa8`, not Radix `#0d74ce`). Cyan and orange are mostly palette-viewer only. Orange-9 and amber-9 are used for terminal art.

### 1.3 Semantic roles (the only layer components use)

Source: `DS/styles/tokens.css:519-662` (semantics), `:671-835` (public roles). Usage text paraphrases `DS/tokens/registry.ts:250-915`.

**Surfaces**

| Token | L | D | Used for |
|---|---|---|---|
| `--surface-base` | `#f5f5f6` | `#000000` | plain background behind panels; window color |
| `--surface-panel` / `--bg-panel` | `#ffffff` | `#1a1a1a` | opaque content panels, cards, timelines, composer |
| `--surface-popover` / `--bg-float` | `#ffffff` | `#282828` | menus, dialogs (opaque fallback) |
| `--surface-elevated-glass` | `rgba(255,255,255,0.90)` | `rgba(40,40,40,0.90)` | floating surfaces when blur is supported |
| `--surface-inset` | `#f5f5f6` | `#101010` | recessed regions: fields, code, unresolved chip |
| `--surface-inverse` | `#191919` | `#c7c7c7` | image/video stage (pair with `text-inverse`) |
| `--surface-categorical-purple` | `#f7edfe` | `#301c3b` | categorical fill |
| `--bg-scrim` | `#00000060` = `rgba(0,0,0,0.376)` | `#000000a6` = `rgba(0,0,0,0.651)` | modal backdrop |
| `--bg-chrome-selected` | `#ffffff` | `#333333` | selected pill inside glass chrome |
| `--bg-app` | gradient-1 (1.6) | gradient-1 (1.6) | full-window backdrop |

**Text**

| Token | L | D | Used for |
|---|---|---|---|
| `--text-standard` = `--text-primary` | `#0f0f0f` | `#ffffff` | primary text and inherited icons |
| `--text-subtle` = `--text-secondary` | `#525252` | `#c7c7c7` | supporting text, author names, labels |
| `--text-metadata` = `--text-tertiary` | `#5f5f5f` | `#a4a4a4` | timestamps, counts, hints, placeholders |
| `--text-unavailable` = `--text-disabled` | `#959595` | `#595959` | disabled controls only |
| `--text-inverse` | `#ffffff` | `#000000` | text on prominent fills, tooltips |
| `--text-danger` | `#641723` | `#ffd1d9` | errors, destructive labels |
| `--text-warning` | `#4f3422` | `#ffe7b3` | warnings |
| `--text-success` | `#193b2d` | `#b1f1cb` | success |
| `--text-link` | `#0b5fa8` | `#83c4ff` | inline links and mentions |
| `--text-accent` / `--text-categorical-purple` | `#402060` | `#ecd9fa` | linked or selected identity text |
| `--text-navigation` | `#404040` | `#c7c7c7` | sidenav labels (Regular) |
| `--text-navigation-unread` | `#262626` | `#ffffff` | unread sidenav labels (weight 500) |
| `--text-on-accent` | `#ffffff` | `#ffffff` | text on `affordance-accent-prominent` |
| `--text-on-media` | `#ffffff` | `#ffffff` | icons on media glass |
| `--text-avatar-fallback` | `#525252` | `#525252` | top-bar profile initials on a white disc |

**Borders**

| Token | L | D | Used for |
|---|---|---|---|
| `--border-standard` = `--border-primary` | `#e8e8e8` | `#232323` | quiet separators, panel edges, floating surface edge |
| `--border-prominent` = `--border-control` | `#808080` | `#737373` | control boundary (outline button, checkbox, focused field) |
| `--border-floating-control` | `#808080` | `#808080` | control boundary on floating surfaces |
| `--border-focus` | `#000000` | `#ffffff` | keyboard focus rings, composer focus |
| `--border-danger` | `#e5484d` | `#e5484d` | invalid field |
| `--border-warning` | `#ab6400` | `#ffc53d` | warning boundary |
| `--border-accent` | `#be93e4` | `#8457aa` | identity accent underline |
| `--border-image` | `rgba(0,0,0,0.1)` (oklch(0 0 0/0.1)) | `rgba(255,255,255,0.1)` (oklch(1 0 0/0.1)) | 1px hairline around message images/videos |
| `--border-media-glass` | `rgba(255,255,255,0.10)` | same | media-glass rim |

**Affordances (control fills and states)**

| Token | L | D | Used for |
|---|---|---|---|
| `--affordance-prominent` | `#000000` | `#ffffff` | primary action fill, checked switch/checkbox, tooltip bg |
| `--affordance-prominent-hover` | `#191919` | `#c7c7c7` | primary hover |
| `--affordance-prominent-pressed` | `#525252` | `#b8b8b8` | primary pressed or expanded |
| `--affordance-subtle` | `#f5f5f6` | `#232323` | secondary fill, chips, avatar fallback |
| `--affordance-subtle-hover` | `#f1f1f2` | `#2e2e2e` | secondary hover, switch track off |
| `--affordance-subtle-pressed` | `#dadada` | `#333333` | secondary pressed |
| `--affordance-selected` | `#e8e8e8` | `#333333` | persistent selection |
| `--affordance-disabled` | `#e8e8e8` | `#1a1a1a` | disabled fill |
| `--affordance-panel-hover` | `#f5f5f6` | `#232323` | quiet nav-row hover on a panel |
| `--affordance-navigation-quiet` | `#f7f7f7` | `#232323` | sidenav gentle hover/selected |
| `--affordance-floating-hover` | `#f5f5f6` | `#333333` | highlighted rows on floating surfaces |
| `--affordance-popover-selected` | `#f5f5f6` | `#404040` | selection in anchored popups (mentions, emoji, menu highlight-radio) |
| `--affordance-floating-control` | `#f5f5f6` | `#404040` | subtle action at rest on floating surface |
| `--affordance-floating-control-hover` | `#f1f1f2` | `#595959` | same, hover |
| `--affordance-floating-control-pressed` | `#dadada` | `#737373` | same, pressed |
| `--affordance-floating-selected` | `#dadada` | `#404040` | selection on floating surface |
| `--affordance-backdrop-hover` | `rgba(0,0,0,0.06)` | `rgba(255,255,255,0.10)` | top-bar ghost hover over gradient |
| `--affordance-backdrop-pressed` | `rgba(0,0,0,0.10)` | `rgba(255,255,255,0.16)` | top-bar pressed and selected |
| `--affordance-danger` | `#feebec` | `#3b1219` | destructive button fill |
| `--affordance-danger-hover` | `#ffdbdc` | `#500f1c` | destructive hover |
| `--affordance-danger-pressed` | `#ffcdce` | `#611623` | destructive pressed |
| `--affordance-warning` | `#fff7c2` | `#302008` | warning fill (`.notice`) |
| `--affordance-success` | `#e6f6eb` | `#132d21` | success fill |
| `--affordance-link-hover` | `#e6f4fe` | `#0d2847` | inline link hover bg |
| `--affordance-accent` | `#f7edfe` | `#301c3b` | quiet identity fill, tint IconButton, tab preview |
| `--affordance-accent-hover` | `#f2e2fc` | `#3d224e` | tint hover/pressed |
| `--affordance-accent-prominent` | `#8e4ec6` | `#8e4ec6` | identity accent fill (white text) |
| `--affordance-accent-prominent-hover` | `#8347b9` | `#975acc` | its hover |

**Status**

| Token | L | D | Used for |
|---|---|---|---|
| `--status-online` | `#218358` | `#3dd68c` | running activity / agent badge ink |
| `--status-availability-online` | `#2b9a66` | `#33b074` | online availability label tint |
| `--status-avatar-online` | `#2b9a66` | `#33b074` | online dot center |
| `--status-avatar-online-border` | `#218358` | `#3dd68c` | online dot 1px outline |
| `--status-away` | `#ffba18` | `#ffd60a` | away dot |
| `--status-offline` | `#808080` | `#a4a4a4` | offline dot |
| `--icon-status-progress` | `#ffba18` | `#ffd60a` | in-progress task icon |
| `--icon-status-complete` | `#2b9a66` | `#33b074` | completed task icon |
| `--art-terminal-start` / `-end` | `#f76b15` / `#ffc53d` | same | terminal welcome lettering gradient |

**Media**: `--bg-media-glass` `rgba(0,0,0,0.35)`, `--bg-media-canvas` `#000000`, `--bg-media-track` `rgba(255,255,255,0.16)` (same in both modes).

**Host compatibility aliases** (`shared/styles/tokens.css`, for legacy callers; same in both modes, they re-point to roles):
`--text`→standard, `--text-muted`→subtle, `--surface`→panel, `--surface-accent`/`--selected`→affordance-selected, `--surface-hover`→subtle-hover, `--surface-control`/`--action`/`--toggle-track`→affordance-subtle, `--surface-input`→panel, `--surface-elevated`→popover, `--border`/`--shell-edge`→border-standard, `--border-input`→border-prominent, `--accent`/`--link`→**text-standard** (not a color accent), `--primary`→prominent, `--on-primary`/`--toggle-thumb`→text-inverse, `--focus`→border-focus, `--danger/--warning/--success`→text roles, `--workspace`→surface-base, `--overlay`→bg-scrim, `--elevation-card`→shadow-xs, other `--elevation-*`→shadow-sm, `--radius-card*`→20px, `--radius-legacy-control`→12px.

### 1.4 Interaction contexts (surface-aware state fills)

Source: `DS/styles/materials.css:148-227`, `components.css:494-506`, `overlays.css:8-9,320-326`, `shared/styles/globals.css:189-192`.
Controls read `--interaction-*`. The value depends on the painted surface owner:
- **panel**: default on `:root`, `.dark`, and `.panel`.
- **floating**: Dialog, Menu, Select, Popover, PreviewCard, Toast (anything using `floating-surface`).
- A nested `.panel` resets to panel. Buttons inside `.buzz-input-group` reset to panel.

| Variable | panel L | panel D | floating L | floating D |
|---|---|---|---|---|
| `--interaction-fill` | `#f5f5f6` | `#232323` | `#f5f5f6` | `#404040` |
| `--interaction-hover` | `#f1f1f2` | `#2e2e2e` | `#f1f1f2` | `#595959` |
| `--interaction-pressed` | `#dadada` | `#333333` | `#dadada` | `#737373` |
| `--interaction-selected` | `#e8e8e8` | `#333333` | `#dadada` | `#404040` |
| `--interaction-border` | `#808080` | `#737373` | `#808080` | `#808080` |
| `--interaction-row-hover` | `#f5f5f6` | `#232323` | `#f5f5f6` | `#333333` |
| `--interaction-row-text` | `#525252` | `#c7c7c7` | `#0f0f0f` | `#ffffff` |
| `--interaction-row-metadata` | `#5f5f5f` | `#a4a4a4` | `#0f0f0f` | `#ffffff` |
| `--interaction-selected-text` | `#525252` | `#c7c7c7` | `#0f0f0f` | `#ffffff` |
| `--interaction-selected-metadata` | `#5f5f5f` | `#a4a4a4` | `#0f0f0f` | `#ffffff` |

Named overrides:

| Context | Override |
|---|---|
| `.shell-header` (top bar) | hover = `rgba(0,0,0,.06)` / `rgba(255,255,255,.10)`; pressed and selected = `rgba(0,0,0,.10)` / `rgba(255,255,255,.16)`. Ghost icon buttons stay transparent at rest. |
| Compact Menu/Popover (`data-size="compact"`) | row-hover = `#f1f1f2` / `#2e2e2e`; selected = `#dadada` / `#333333` |
| `popover-surface` utility | `--affordance-selected` → popover-selected `#f5f5f6` / `#404040` |
| `interaction-navigation` (sidebar) | selected = affordance-selected `#e8e8e8` / `#333333` |
| `interaction-availability` (status capsule, `data-status`) | caption weight → 500. Fill/hover/pressed = `color-mix(in srgb, C p%, S)` with p = 12/18/24. **online**: C = green-10, S = surface-inset → L `#ddeae5` / `#d1e5dc` / `#c5dfd3`, D `#14231c` / `#162d22` / `#183628`; text = C (`#2b9a66` / `#33b074`). **away**: C = amber-10, S = transparent → L `rgba(255,186,24,.12/.18/.24)`, D `rgba(255,214,10,.12/.18/.24)`; text = text-warning. **offline/unknown**: C = text-subtle → L `#e1e1e2` / `#d8d8d8` / `#cececf`, D `#262626` / `#313131` / `#3c3c3c`; text = text-subtle. |

### 1.5 Spacing, sizes, radii

Source: `DS/styles/tokens.css:370-386,724-767`.

| Space token | px | | Role token | → | px |
|---|---|---|---|---|---|
| `--space-half` | 2 | | `--space-workspace-inset` | space-4 | 16 |
| `--space-1` | 4 | | `--space-panel-gap` | space-2 | 8 |
| `--space-1h` | 6 | | `--space-panel-inset` | space-6 | 24 |
| `--space-2` | 8 | | `--space-control-inset` | space-4 | 16 |
| `--space-3` | 12 | | `--space-row-gap` | space-2 | 8 |
| `--space-4` | 16 | | `--space-section-gap` | space-8 | 32 |
| `--space-5` | 20 | | `--space-page-section-gap` | space-16 | 64 |
| `--space-6` | 24 | | `--space-chip-inset` | 0.1875rem | 3 |
| `--space-8` | 32 | | `--space-chip-gap` | space-1 | 4 |
| `--space-16` | 64 | | | | |

The legacy Tailwind spacing unit is `--spacing: 4px` (so `p-2` = 8px).

| Size token | px | | Radius token | px | Use |
|---|---|---|---|---|---|
| `--size-topbar` | 56 | | `--radius-chip` | 4 | inline chips, links, checkbox (4px) |
| `--size-panel-header` | 40 | | `--radius-row` (corner-row) | 8 | rows, nav items, compact popups |
| `--size-control-sm` / `--size-row` | 32 | | `--radius-icon-button` | 10 | all IconButtons, nav tabs |
| `--size-control` | 40 | | `--radius-control` | 12 | fields, `shape="control"` buttons |
| `--size-control-lg` | 52 | | `--radius-container` | 16 | floating surfaces, settings groups, empty state |
| `--size-navigator` | 216 | | `--radius-panel` | 20 | panels, dialogs, composer |
| `--size-resize-seam` | 14 | | `--radius-capsule` | 26 | text Buttons (pill up to 52px) |
| `--size-emoji-display/line/inline` | 42 / 48 / 22 | | `--radius-pill` | 9999rem | pills, circles, switches, tooltips |

Nested radius rule: `inner = max(0, outer − inset − border)`. `--floating-inner-radius` resolves to:
- 11px for default menus and selects (16 − 4 − 1)
- 5px for compact menus and compact list popovers (8 − 2 − 1)
- 3px for content popovers (16 − 12 − 1)
- 15px for `padding="none"` (16 − 0 − 1)

Note: DESIGN.md mentions "3px compact", but the CSS uses a 2px compact inset, which gives 5px.

Legacy Tailwind radii (`shared/styles/globals.css:58-64`): xs/sm/md = 4, lg = 8, xl = 12, 2xl/3xl = 20.

### 1.6 Backdrop gradients, glass, materials

Sources: `DS/styles/tokens.css:316-361,437-490`, `DS/styles/materials.css`.

**`--bg-app` (gradient-1)** is painted on `body`, `.shell-background`, and the workspaces. CSS layers are listed top-first.

L "Sky field":
```
radial-gradient(at 88% 84%, #f5e99e 0%, transparent 60%),
linear-gradient(148deg, #f8fbff 0%, #c6e8fb 50%, #d9f0d5 100%)
```
D "Night garden":
```
radial-gradient(ellipse 110% 100% at 52% 50%, transparent 34%, rgba(2,11,13,0.42) 100%),
radial-gradient(at 88% 16%, rgba(52,211,153,0.56) 0%, transparent 42%),
radial-gradient(at 12% 86%, rgba(56,189,248,0.52) 0%, transparent 46%),
radial-gradient(at 52% 48%, rgba(6,95,70,0.30) 0%, transparent 78%),
linear-gradient(153deg, #071311 0%, #0a2929 51%, #081722 100%)
```
`radial-gradient(at X Y, ...)` with no size uses `farthest-corner` and an ellipse shape. `body` uses `background-attachment: fixed`.
Unused paired slots (verbatim in `tokens.css:341-351,459-488`): gradient-2 Peach field / Signal flare, gradient-3 Blue hour / Electric dusk, gradient-4 Orchid field / Ultraviolet.

The `.shell-header` background is `color-mix(in srgb, var(--bg-app) 72%, var(--surface-panel))` (`shared/styles/globals.css:199`). `--bg-app` is an image, so this is invalid at computed-value time and the header **renders transparent** over the gradient.

Texture (dots, unused in product chrome): `radial-gradient(color 1px, transparent 1px)` on a 28px tile. Color L `rgba(10,10,10,0.059)`, D `rgba(255,255,255,0.078)`.

**Glass ramp** (fill only):

| Token | L | D | Job |
|---|---|---|---|
| glass-1 | `rgba(255,255,255,.361)` | `rgba(28,28,28,.361)` | barely there |
| glass-2 = `--bg-glass-primary` | `rgba(255,255,255,.522)` | `rgba(28,28,28,.522)` | quiet glass |
| glass-3 = `--bg-glass-primary-hover` | `rgba(255,255,255,.620)` | `rgba(28,28,28,.659)` | default/hover |
| glass-4 = `--bg-glass-secondary` | `rgba(255,255,255,.780)` | `rgba(28,28,28,.780)` | glass over glass |
| glass-5 = `--bg-glass-secondary-hover` | `rgba(255,255,255,.878)` | `rgba(28,28,28,.878)` | nearly solid |
| `--rim-lit` | `rgba(255,255,255,.451)` | `rgba(255,255,255,.180)` | top inner highlight |
| `--rim-shade` | `rgba(255,255,255,.122)` | `rgba(255,255,255,.059)` | bottom inner highlight |

Blur scale: `--blur-sm` 8px, `--blur-md` 16px, `--blur-lg` 32px.

**Material recipes**:

| Utility | background | backdrop-filter | box-shadow / border |
|---|---|---|---|
| `glass-primary` (chrome tabs, chrome nav, chrome IconButton) | glass-2 (hover in `-interactive`: glass-3; blur never changes) | `blur(16px)` | `inset 0 1px 0 rim-lit, inset 0 -1px 0 rim-shade` |
| `glass-secondary` | glass-4 (hover glass-5) | `blur(32px)` | `shadow-sm, inset 0 1px 0 rim-lit, inset 0 -1px 0 rim-shade` |
| `elevated-material` (dialogs, floating) | `surface-elevated-glass` L `rgba(255,255,255,.9)` / D `rgba(40,40,40,.9)`; opaque fallback `surface-popover` (L `#fff`, D `#282828`) for no-blur, reduced transparency, forced colors | `blur(8px)` | — |
| `floating-surface` (Menu, Select, Popover, PreviewCard, Toast) | elevated-material + interaction-floating | `blur(8px)` | border `1px solid border-standard`; radius `--floating-radius` 16px; `shadow-sm`; vars `--floating-inset` 4px |
| `media-glass` (video controls) | `rgba(0,0,0,.35)` | `blur(24px) saturate(1.5)` | border `1px solid rgba(255,255,255,.10)`; text `#fff`; no shadow |
| `.panel` | `surface-panel` | — | `1px solid border-standard`, radius 20, `shadow-xs`. A nested panel inside `[data-joined]` drops its border, radius, and shadow. |

### 1.7 Elevation, motion, z-index

Source: `DS/styles/tokens.css:363-364,389-396,737-740,768-775`.

| Shadow | Value (identical L and D) | Used by |
|---|---|---|
| `--shadow-xs` | `0 1px 2px rgba(0,0,0,0.05)` | panel, composer, switch thumb, chrome selected pill |
| `--shadow-sm` | `0 0 2px rgba(0,0,0,0.1), 0 4px 8px -2px rgba(0,0,0,0.06)` | floating surfaces, dialogs, tooltip, glass-secondary, mobile nav dock |

| Duration | ms | | Easing | Value |
|---|---|---|---|---|
| `--duration-fast` | 120 | | `--easing-state` | `ease` |
| `--duration-state` / `--duration-field-focus` | 150 | | `--easing-enter` | `cubic-bezier(0.23, 1, 0.32, 1)` |
| `--duration-panel-enter` | 180 | | `--easing-settle` | `cubic-bezier(0.645, 0.045, 0.355, 1)` |
| `--duration-settle` | 220 | | | |

Derived timings:

| Surface | Enter | Exit |
|---|---|---|
| Menu / Popover | 75ms; opacity and blur use `ease`, transform uses settle; 2px offset and 2px blur toward the trigger | 60ms |
| Select / Combobox popup | 150ms; 4px offset + 4px blur | 120ms |
| Tooltip | 150ms from scale .97, translateY 2px, blur 2px | 120ms |
| Dialog | opacity 150ms ease + transform 220ms settle from scale .98 | 120ms ease |

Other timings:
- Dialog step crossfade: 200ms `cubic-bezier(.2,.8,.2,1)` with 4px blur.
- PreviewCard: opacity and transform 150ms, from scale .98.
- Toast: opacity fade 150ms.
- Switch thumb and tab indicator: 220ms settle.
- Workspace tab arrival flash: 660ms ease-out.
- Launch overlay fade: 220ms ease-out.
- Sidebar collapse: 220ms ease-in-out.
- Button spinner: 1s linear rotation.

Instant cases: keyboard navigation (`html[data-keyboard-navigation]`) and `prefers-reduced-motion` remove transform, blur, and transitions. Tooltip keeps the fade under reduced motion.

| z-index layer | Value |
|---|---|
| launch overlay | 10000 |
| `--layer-tooltip` | 1200 |
| `--layer-popover` (menus, popovers, select, preview cards) | 1100 |
| `--layer-dialog` (also backdrop of right-sheet and outside-dismiss dialogs) | 1001 |
| `--layer-dialog-backdrop` | 1000 |
| toast viewport | 999 |
| mobile nav overlay | 20 |

### 1.8 Typography

Sources: `DS/styles/typography.css`, `shared/styles/globals.css:27-56,87-176,409-459`, `src/main.tsx:8-10`.

**Families**:

| Family | Stack | Loaded from |
|---|---|---|
| Sans | `"Inter Variable", Inter, ui-sans-serif, system-ui, -apple-system, sans-serif` | `@fontsource-variable/inter` (wght axis, roman + italic) |
| Mono | `"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace` | `@fontsource/jetbrains-mono` 400 (terminal also loads 600) |
| Emoji | `"Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", sans-serif` | system |

**Rendering**:
- `:root { -webkit-font-smoothing: antialiased; font-synthesis: none }`.
- `[data-buzz-ui]` sets `text-rendering: optimizeLegibility; font-synthesis-weight: none`.
- No `font-feature-settings` anywhere, so Inter defaults apply.
- `code, kbd, pre, samp` use mono with `font-variant-numeric: tabular-nums`.
- Weights: only 400 (Regular) and 500 (Medium). `font-semibold` and `strong/b` resolve to 500. The one exception is `--weight-profile-status: 600`.

**Type roles** (px at scale 1; letter-spacing in em, with px in parentheses):

| Role | Size | Line height | Tracking | Weight | Use |
|---|---|---|---|---|---|
| `text-display` | 56 | 1.1 (61.6) | −0.04em (−2.24) | 400 | hero |
| `text-title` | 32 | 1.1 (35.2) | −0.015em (−0.48) | 500 | screen title, legacy `h1` |
| `text-heading` | 24 | 1.1 (26.4) | −0.0075em (−0.18) | 500 | section title, AlertDialog title, legacy `h2` |
| `text-body-lg` | 20 | 1.4 (28) | −0.01em (−0.2) | 400 | lead paragraph |
| `text-label` | 16 | 1.5 (24) | 0 | 500 | Dialog/Panel titles, popover titles, chrome nav pills, legacy `h3`/buttons |
| `text-body` | 14 | 20/14 (20) | −0.005em (−0.07) | 400 | **default body**; reading text, fields |
| `text-body-sm` | 14 | 20 | −0.0025em (−0.035) | 400 | dense text, menus, descriptions |
| `text-label-sm` | 14 | 20 | +0.0025em (+0.035) | 500 | Button labels, field labels, row labels |
| `text-caption` | 12 | 16/12 (16) | +0.0133em (+0.16) | 400 | tooltips, timestamps, small avatar initials |
| `text-mono` (= `mono-lg` = `mono-sm`) | 12 | 16 | +0.03em (+0.36) | 400 | code, identifiers (JetBrains Mono) |

Legacy Tailwind size aliases (size + line height only):
- xs → caption
- sm → body-sm
- base → body
- lg and xl → body-lg
- 2xl → heading
- 3xl and 4xl → title
- 5xl through 9xl → display

Legacy un-marked elements (`:not([data-buzz-ui])`):
- `h1` = title, margin 8px 0 32px
- `h2` = heading, margin 32px 0 16px
- `h3` = label, margin 0 0 6px
- `p` margin 8px 0

**Scaling**:
- The host sets root `font-size` = 100% × interface scale (0.8–2.0), which drives every rem.
- `--type-scale` (`:root[data-font-size="smaller"]` 0.9 / `"larger"` 1.1) also exists, but nothing in the app sets `data-font-size`. Treat it as 1.

### 1.9 Focus, selection, scrollbars

**Focus visibility gate** (`DS/useKeyboardFocusVisibility.ts`):
- `html[data-keyboard-navigation]` is set on keydown of Tab, Arrow keys, Home, End, PageUp, or PageDown. It is not set when meta or ctrl is held. Alt only counts with Tab.
- The attribute is removed on `pointerdown` and `touchstart`.
- Rings render only when that attribute is present AND the element is `:focus-visible`. Pointer focus shows nothing.

**Ring recipes** (all `outline: 2px solid`):

| Ring color | Offset | Applies to |
|---|---|---|
| `text-standard` (L `#0f0f0f`, D `#fff`) | 2px | Button/IconButton, NavigationItem, tabs, switch track, accordion trigger, preview-card example |
| `border-focus` (L `#000`, D `#fff`) | 2px | read-only inputs, checkbox, radio, tab panel, toast, inline chip, inline link |
| `border-focus` | **−2px** (inset) | menu items, navigation-variant tabs, `option` NavigationItem, compact popover rows |
| `--focus` | 3px | legacy non-DS controls |

Exceptions:
- Editable `.buzz-input`, `.buzz-textarea`, and rich textboxes have **no ring** (`outline:none !important`). Focus is shown by the field border instead (6.3).
- Composer: `border-color: border-focus` plus `box-shadow: shadow-xs, 0 0 0 1px border-focus` on `:focus-within`. This is not modality-gated. The stroke goes L `#e8e8e8`→`#000000`, D `#232323`→`#ffffff`.
- SearchField capsule has no visible focus. This is a documented gap; reproduce it as-is.

**Selection**: no custom `::selection`. The platform `Highlight` / `HighlightText` is used, and the composer sets it explicitly. Caret = `text-standard`.

**Scrollbars** (`DS/styles/scrollbars.css`):
- Every element uses `scrollbar-width: thin; scrollbar-color: var(--scrollbar-thumb-quiet) transparent`.
- `--scrollbar-thumb-quiet` = `color-mix(in srgb, border-standard 90%, transparent)` = L `rgba(232,232,232,0.9)`, D `rgba(35,35,35,0.9)`. The track is transparent.
- Width is the platform "thin" width.
- `--scrollbar-thumb` (L `#808080`, D `#808080`) is defined but not used by the universal recipe.
- Navigation tab strip: horizontal-only scrollbar, 4px tall. The thumb has a 1px transparent top border, so 3px is visible. Pill radius. The thumb is transparent until the strip is hovered (always visible on touch devices).

## 2. Shell layout

Source root: `/home/ben/projects/buzz-app` (all paths below are relative to it).

### 2.0 Units, root size and conventions

- Root font size is `calc(100% * var(--buzz-text-scale, 1))`
  (`src/shared/styles/globals.css:409-413`). The browser default is 16px, so at the
  default scale **1rem = 16px**. Appearance > Interface size changes the scale from 0.8 to
  2.0 in 0.1 steps (`src/shared/theme/service.ts:6-17`). All `rem` sizes (header height,
  rail width, row heights, paddings) scale with it. Values given in `px` in CSS (sidebar
  widths 220/260/520, the 336px clamp, the 650px breakpoint) do **not** scale.
- Media queries in `rem` use the browser default (16px), not the scaled root, so `40rem`
  is always 640px.
- Tailwind spacing unit: `--spacing: var(--space-1)` = 0.25rem, so `p-2` = 8px and
  `pt-14` = 56px (`globals.css:28`).
- Spacing tokens (`src/shared/design-system/styles/tokens.css:370-381`): `space-half` 2,
  `space-1` 4, `space-1h` 6, `space-2` 8, `space-3` 12, `space-4` 16, `space-5` 20,
  `space-6` 24, `space-8` 32, `space-16` 64 (px).
- Size tokens (`tokens.css:741-752`): `size-topbar` 56 (not used by the shell header),
  `size-panel-header` 40, `size-control-sm` 32, `size-control` 40, `size-control-lg` 52,
  `size-row` 32 (24 inside the channel sidebar, see 3.1), `size-navigator` 216,
  `size-resize-seam` 14 (unused by the shell; its handle is 16px).
- Radius tokens (`tokens.css:382-387, 753-770`): `radius-row` 8, `radius-control` 12,
  `radius-container` 16, `radius-panel` 20, `radius-icon-button` 10, `radius-capsule` 26,
  `radius-chip` 4, `radius-pill` 9999.
- Font: `"Inter Variable", Inter, ui-sans-serif, system-ui, ...`
  (`src/shared/design-system/styles/typography.css:39-40`). The type roles used here:

| Role | Size / line-height | Weight | Letter-spacing |
|---|---|---|---|
| `text-label` | 16 / 24 | 500 | 0 |
| `text-label-sm` | 14 / 20 | 500 | 0.0025em |
| `text-body` | 14 / 20 | 400 | -0.005em |
| `text-body-sm` | 14 / 20 | 400 | -0.0025em |
| `text-caption` | 12 / 16 | 400 | 0.0133em |
| `text-heading` | 24 / 26.4 | 500 | -0.0075em |

(`typography.css:43-78`.) Medium is 500; the channel sidebar redefines "semibold" to 600
(see 3.1).

- Colors are named by token. Resolved light / dark values are given where they matter.
  The canonical resolution is in `tokens.css:509-660`.

### 2.1 Overall structure

`src/app/shell/AppShell.tsx:230-450` renders this DOM tree:

```
div.shell-background            full window, flex column, h=100dvh, overflow hidden
  header.shell-header           min-height 48px
    div.shell-communities       left group: sidebar toggle + back/forward
    nav.shell-topbar-pages      absolutely centered pill navigation ("Me", "Messages")
    div.shell-actions           right group (margin-left: auto)
      nav.shell-toolbar-pages   toolbar page icons (none by default)
      [More pages]              overflow button, only when compact
      div.shell-actions-fixed   panel launchers, Search, Profile, Window controls
  div.flex.min-h-0.flex-1       body row
    nav.rail (CommunityRail)    48px wide
    div.shell-body              flex-1, right/bottom gutter
      Panel[joined]             ONE rounded card (radius 20, border 1)
        div.shell-content       flex row, container-type inline-size
          div.shell-navigation  sidebar region (collapsible on some pages)
            [sidebar 260px] [hr resize handle]
          main#main-content     flex-1
            PanelFrame          page | companion dock
```

#### ASCII diagram (1200x800 window, Linux/Windows, scale 1.0, width >= 640px, Messages page)

```
x=0   32  64  96 128                     ~600 (center)                 1000 1040  1080        1184 1200
+------------------------------------------------------------------------------------------------+ y=0
| pad16+16 [tog][<-][->]          ( glass pill 40h:  [ Me ] [Messages] )       [Q] [@]  [-][^][x]  pad16|  header 48px
+------+-----------------------------------------------------------------------------------------+ y=48
| rail | +---------------------------------------------------------------------------------+     |
| 48px | |  sidebar 260px (default)     |1px|  main content                                |     |
|      | |  Panel fill surface-panel    | d |  (PanelFrame: page, optional dock 420px)     |     |
| [40] | |  brand mark                  | i |                                              |     |
|  gap | |  page rows (Inbox...)        | v |                                              | 16  |
| [40] | |  section: Starred            | i |                                              | gut-|
|  8   | |  section: Channels           | d |                                              | ter |
| [40] | |  section: Direct messages    | e |                                              |     |
|  ... | |                              | r |                                              |     |
| [+]  | +---------------------------------------------------------------------------------+     |
|      |   joined Panel: x 48..1184, y 48..784, radius 20, border 1px border-standard          |
+------+------------------------------------------------------------------- 16px bottom gutter --+ y=800
```

Exact geometry at this size:

| Element | X range (px) | Y range (px) | Notes |
|---|---|---|---|
| Header | 0..1200 | 0..48 | `min-height: 3rem` |
| Sidebar toggle (when shown) | 32..64 | 8..40 | header `padding-left` 16 + `.shell-communities` `padding-left` 16 |
| Back / Forward | 64..96 / 96..128 | 8..40 | `gap: 0` (`NavigationControls.tsx:12`) |
| Window controls (3 x 32, gap 4) | 1080..1184 | 8..40 | header `padding-right` 16 |
| Profile avatar | 1040..1072 | 8..40 | 8px gap before the window controls |
| Search | 1000..1032 | 8..40 | |
| Community rail | 0..48 | 48..800 | items start at y=56 (`padding-top` 8) |
| Joined Panel | 48..1184 | 48..784 | `shell-body` `pr-4 pb-4` (16), `pl-0` |
| Sidebar | 48..308 | 48..784 | 260px including its 1px right border |
| Main | 308..1184 | 48..784 | |

If the sidebar toggle is not shown (pages other than Messages, Agents, Me, Settings), Back
starts at x=32. On macOS desktop, add 64px to the left of the first control (see 2.3).

### 2.2 Window background (behind header, rail and gutters)

- `.shell-background { background: var(--bg-app); }` (`globals.css:185-187`). The utility
  `bg-shell` also sets `background-color: var(--workspace)` (`surface-base`: light
  `#f5f5f6`, dark `#000000`) under the image (`AppShell.tsx:233`, `globals.css:69`).
- `--bg-app` = `--gradient-1` (`tokens.css:672`, `:503`):
  - Light "sky-field" (`tokens.css:338-340`):
    `radial-gradient(at 88% 84%, #f5e99e 0%, transparent 60%), linear-gradient(148deg, #f8fbff 0%, #c6e8fb 50%, #d9f0d5 100%)`
  - Dark "night-garden" (`tokens.css:449-457`): vignette
    `radial-gradient(ellipse 110% 100% at 52% 50%, transparent 34%, rgba(2,11,13,.42) 100%)`,
    then `radial-gradient(at 88% 16%, rgba(52,211,153,.56) 0%, transparent 42%)`,
    `radial-gradient(at 12% 86%, rgba(56,189,248,.52) 0%, transparent 46%)`,
    `radial-gradient(at 52% 48%, rgba(6,95,70,.3) 0%, transparent 78%)`,
    `linear-gradient(153deg, #071311 0%, #0a2929 51%, #081722 100%)`.
- The gradient covers the whole window and is visible through the header, the rail and the
  outer gutters. `docs/shell-design.md` mentions `public/shell-gradient.png` and a dot
  texture. No code in `src/` references either; the CSS gradient is what renders.

### 2.3 Header / title bar

`globals.css:189-216, 331-362`; `AppShell.tsx:247-382`.

| Property | Value |
|---|---|
| Height | `min-height: 3rem` (48px). `flex-wrap: wrap`, so it can grow if the groups wrap. |
| Layout | flex, `align-items: center`, `justify-content: space-between`, `gap` 8px |
| Padding-left | 16px (8px at `max-width: 43.75rem` = 700px) |
| Padding-right | 8px below 640px, 16px at `min-width: 40rem` (640px) |
| Background | Declared as `color-mix(in srgb, var(--bg-app) 72%, var(--surface-panel))` (`globals.css:199`). `--bg-app` is a gradient list, and `color-mix()` accepts only colors, so the declaration is invalid at computed-value time and the header renders **transparent** over the shell gradient. Port: give the header no fill. Check this against a screenshot. |
| Interaction overrides | Inside the header: `--interaction-hover: --affordance-backdrop-hover` (light `rgb(0 0 0 / 6%)`, dark `rgb(255 255 255 / 10%)`); `--interaction-pressed` and `--interaction-selected` are `--affordance-backdrop-pressed` (light `rgb(0 0 0 / 10%)`, dark `rgb(255 255 255 / 16%)`) (`globals.css:190-192`, `tokens.css:316-317, 437-438`). |
| Icon buttons | Every `IconButton` in the header is forced to 32x32 (`size-control-sm`) with 16x16 SVGs (`globals.css:336-353`). Ghost buttons have a transparent fill except on hover, focus-visible or active. Corner radius is `radius-icon-button` 10px (`components.css:179-186`). Fill transition: 150ms ease (`components.css:27-29`). |
| Drag region | Linux/Windows: `data-tauri-drag-region` on the header, `.shell-communities` and `.shell-actions` backgrounds. macOS: a custom mousedown handler starts the drag; double-click follows the system title-bar preference (`src/app/shell/title-bar.ts`). Controls stay clickable. |

**Left group** `.shell-communities` (`globals.css:206-216`): flex, `gap: 0`,
`padding-left` 16px. On macOS desktop (`.shell-header-mac`) the padding-left is
`space-16 + space-4` = 80px, so the first control starts 96px from the window edge. The
native traffic lights are placed by Tauri at `trafficLightPosition {x: 20, y: 26}`
(`src-tauri/tauri.conf.json:19-24`; `titleBarStyle: Overlay`, `hiddenTitle: true`).
The browser build has no inset and no imitation controls.

Contents, left to right:
1. **Sidebar toggle**: ghost IconButton with Tabler `IconLayoutSidebar`, 16px. It is
   rendered only when the page has a collapsible sidebar (Messages
   `buzz.channels/channels`, Agents `buzz.agents/agents`, Me `buzz.me/me`, Settings) or
   when the window is <=650px wide (`AppShell.tsx:80-106, 260-277`). Labels: "Hide/Show
   Channel sidebar", "Hide/Show Me sidebar", or "Hide/Show navigation" when narrow. It sets
   `data-highlight-expanded="false"`, so `aria-expanded=true` does **not** paint the
   pressed fill (`components.css:133-147`).
2. **Back / Forward** (`src/app/shell/NavigationControls.tsx`): ghost IconButtons with
   `IconArrowLeft` and `IconArrowRight`, 16px, gap 0. Disabled when there is no history.
   A disabled ghost button has a transparent fill and `text-unavailable` color
   (`components.css:296-312`; light `#959595`, dark `#595959`).

**Center: topbar pages** (`globals.css:217-226`, `components.css:927-956`). Pages with
`placement: "topbar"` are shown here: **Me** then **Messages** (`src/bundled/me/index.tsx:20`,
`src/bundled/channels/index.tsx:48`, order from `src/app/shell/presentation.ts:23-30`).
- Container `nav.chrome-navigation`: `position: absolute; left: 50%; translateX(-50%)`,
  height 40px, padding 4px, gap 4px, `radius-pill`, material `glass-primary`: fill
  `--bg-glass-primary` (light `#ffffff85`, dark `#1c1c1c85`), `backdrop-filter: blur(16px)`,
  plus inset rim shadows `inset 0 1px 0 var(--rim-lit), inset 0 -1px 0 var(--rim-shade)`
  (light `#ffffff73` / `#ffffff1f`, dark `#ffffff2e` / `#ffffff0f`)
  (`src/shared/design-system/styles/materials.css:33-39`, `tokens.css:717, 805-806, 817, 833-834`).
- Each pill: height 32px, padding `0 16px`, `radius-pill`, `text-label` (16px / 500),
  color `text-subtle` (light `#525252`, dark `#c7c7c7`), max-width 12rem (192px). No icon.
- Hover (not selected): fill `--bg-glass-primary-hover` (light `#ffffff9e`, dark
  `#1c1c1ca8`), text `text-standard`.
- Selected: fill `--bg-chrome-selected` (light `#ffffff`, dark `#333333`), text
  `text-standard`, `box-shadow: var(--shadow-xs)` = `0 1px 2px rgba(0,0,0,.05)`.

**Right group** `.shell-actions` (`globals.css:331-335`): flex, `margin-left: auto`,
gap 8px.
1. Toolbar pages (`placement: "toolbar"`; none of the bundled pages use it): 32px chrome
   IconButtons (glass material). An optional badge sits at the top-right corner, max
   32x16 (`globals.css:237-249`).
2. **More pages** (`IconDots`, 16px, ghost): shown only in compact mode. It opens a compact
   popover listing the header pages as navigation rows (`AppShell.tsx:317-364`).
3. `.shell-actions-fixed` (flex, gap 8px):
   - **Panel launchers** (`src/app/shell/PanelLaunchers.tsx`): chrome IconButtons whose
     image comes from the plugin. Fallback icon is `IconLayoutSidebar`.
   - **Search** (`src/app/shell/PageSearch.tsx:155-165`): ghost IconButton, `IconSearch`
     16px, tooltip "Search Buzz (Ctrl+K)" (Cmd+K on macOS). This is an icon button, not
     an inline text field. It opens a modal dialog: width `min(40rem, 100vw - 2rem)`, top
     `clamp(3rem, 17vh, 10rem)`, input placeholder "Search pages, conversations and
     messages…", result rows min-height 44px (`src/app/shell/SearchChoices.tsx:47`,
     `src/app/shell/SearchChoices.css:2-9, 113-116`).
   - **Profile button** (see 2.5).
   - **Window controls** (Linux/Windows desktop only; see 2.4).

**Compact mode**: the header measures its groups. When
`2 * (max(leftWidth, rightWidth) + gap) + centerWidth > header.clientWidth`, or the window
is <=650px wide, the topbar and toolbar pages are hidden (`visibility: hidden`, inert) and
the More pages button appears (`AppShell.tsx:172-212`, `globals.css:250-258`).

### 2.4 Window controls (Linux / Windows)

`src/app/shell/WindowControls.tsx`. These render only when Tauri runs on Linux or Windows
(`navigator.platform` matches `/^(Linux|Win)/`). The windows are undecorated
(`src-tauri/tauri.linux.conf.json:9`, `tauri.windows.conf.json:9`; min size 480x400,
default 1200x800).

| Control | Tabler icon | Size | Variant |
|---|---|---|---|
| Minimize | `IconMinus` | 32x32 button, 16px icon | ghost (default) |
| Maximize / restore | `IconArrowsMaximize` (the icon does not change when maximized) | same | ghost |
| Close | `IconX` | same | ghost |

- Container: `fieldset`, flex, `gap: 4px` (`gap-1`), no border or padding (Tailwind
  preflight).
- Hover fill is the header backdrop hover (light `rgb(0 0 0/6%)`, dark
  `rgb(255 255 255/10%)`); pressed is 10% / 16%. **Close has no red hover.** Radius 10px.
  Icon color `text-standard` (light `#0f0f0f`, dark `#ffffff`).
- Hyprland hides Minimize; Omarchy also hides Maximize, through the
  `window.__BUZZ_WINDOW_CONTROLS__` policy. Close is always shown.
- During launch, a fixed 48px header (`h-12`, `px-4`, controls right-aligned, z 10001)
  carries the same controls (`src/app/shell/LaunchWindowControls.tsx:16-22`).
- If a window action fails, an inline `text-body-sm` alert, "Window action failed. Try
  again.", appears before the buttons.

### 2.5 Profile button and account menu

`src/app/shell/ProfileButton.tsx:217-243`, `ProfileButton.module.css`.

- Trigger: IconButton `variant="avatar"`, `shape="round"`, forced to 32x32 in the header,
  fully round. The Avatar fills the button (`size="fill"`, circle).
- Image: the local default profile picture (HTTPS). Fallback: the first letter of the name
  in uppercase, 14px, on a **white** circle with text `#525252`
  (`--text-avatar-fallback`). With no name, a `IconUser` 19px glyph
  (`ProfileButton.module.css:14-22`).
- Status dot (local intent Online, Away or Offline): see 3.4 for the dot geometry. On a
  32px avatar the dot is 8px, centered at (27.2, 27.2).
- Menu (`ProfileButton.module.css:84-99`): width 288px (18rem), padding 12px, aligned to
  the end, 8px below the trigger. Floating surface: radius 16, border 1px border-standard,
  `shadow-sm`, popover glass fill (`materials.css:67-79`). Contents in order:
  1. Header row: 40px avatar, name in `text-body`, availability chip (`Button` xs, radius
     4px, caption 12px weight 600, green or amber tint).
  2. Status card: inset fill, border, radius 12.
  3. Separator.
  4. Plugin account actions (`IconPlug` 17px fallback).
  5. **Settings** (`IconSettings` 16px; `IconCheck` 14px trailing when Settings is open).
  
  Direct menu rows: min-height 28px, gap 12px, `text-body` 14px.

### 2.6 Community rail

`src/features/communities/CommunityRail.tsx:192-240`,
`src/features/communities/CommunityRailItem.tsx:153-173`,
`src/features/communities/Communities.module.css:21-41`.

| Property | Value |
|---|---|
| Width | 48px (3rem), `flex: 0 0 auto` |
| Padding | 8px top/bottom, 4px left/right |
| Item stack | flex column, `align-items: center`, gap 8px, `overflow-y: auto` (scrolls on its own) |
| Background | none; sits on the shell gradient, outside the Panel |
| Items in order | 1. **Personal space** 2. each saved community (membership order) 3. **Add a community** |
| Item button | ghost IconButton, default size **40x40**, radius 10px |
| Personal space glyph | `IconWorld`, `strokeWidth 1.5`. Requested size 22, but the CSS rule `.buzz-button-label > svg { width/height: 24px }` renders it at **24x24** (`components.css:44-48`) |
| Add glyph | `IconPlus`, rendered at 24x24 for the same reason |
| Community glyph | `Avatar size="small"` = **24x24 squircle** (mask `avatar-squircle.svg`). Image is the NIP-11 icon. Fallback: uppercase first letter, `text-caption` 12px, on `affordance-subtle` (light `#f5f5f6`, dark `#232323`) with `text-subtle` |
| Selected | `data-selected` on the ghost button: fill `--interaction-selected` = `affordance-selected` (light `#e8e8e8`, dark `#333333`). There is no side bar indicator, pill or ring |
| Hover | `--interaction-hover` = `affordance-subtle-hover` (light `#f1f1f2`, dark `#2e2e2e`) |
| Pressed | `affordance-subtle-pressed` (light `#dadada`, dark `#333333`) |
| Tooltip | community name, `side="right"`, no delay |
| Unread dot | **none**. The rail does not show unread totals (`docs/shell-design.md`, "the rail still claims no unread total") |
| Context menu | right-click, the ContextMenu key or Shift+F10. Items: Mark all as read (`IconChecks`), separator, Copy community URL (`IconLink`), Invite to community (`IconTicket`, admins only, selected community only), Community settings (`IconSettings`), separator, Leave community (`IconLogout`, danger tone). Menu icons 14px; compact menu size |

The rail's hover and selected fills are opaque neutrals. They are not the translucent
header fills.

### 2.7 Body, joined Panel, gutters

- `.shell-body` (`globals.css:299-301`): `flex: 1`, `padding: 0 8px 8px 0` below 640px and
  `0 16px 16px 0` at 640px and up. The rail provides the left spacing, so there is no left
  padding. The top edge touches the header.
- Joined `Panel` (`src/shared/design-system/ui/Panel.tsx`, `components.css:494-515`): this
  is the single **inset rounded card on the tinted background**. It fills the body.
  - fill `surface-panel` (light `#ffffff`, dark `#1a1a1a`)
  - `border: 1px solid border-standard` (light `#e8e8e8`, dark `#232323`)
  - `border-radius: 20px` (`radius-panel`), `overflow: hidden` (children are clipped to
    the radius)
  - `box-shadow: var(--shadow-xs)` = `0 1px 2px rgba(0,0,0,0.05)`
  - Nested Panels (sidebar frame, Settings) have `border: 0; border-radius: 0;
    box-shadow: none` and keep their opaque fill. Layout owners draw 1px
    `border-standard` dividers.
- `.shell-content`: flex row, `container-type: inline-size` (`globals.css:302-305`).

### 2.8 Sidebar region: widths, resizing, collapse

| Item | Value | Source |
|---|---|---|
| Default width | 260px | `src/bundled/channels/useSidebarView.ts:4`, `globals.css:311` |
| Min width | 220px | `useSidebarView.ts:5`, `globals.css:312` |
| Max width (JS clamp) | 520px | `useSidebarView.ts:6-13` |
| Max width (CSS) | `max(220px, calc(100% - 336px))`; `100cqi - 336px` when collapsible. Main always keeps at least 336px | `globals.css:313, 324` |
| Right divider | `border-inline-end: 1px solid border-standard`, inside the width (border-box) | `globals.css:310` |
| Persistence | localStorage key `buzz-view.v1:["<scope>","channel-sidebar"]`. JSON `{collapsed: string[], scrollTop, width, location}`. Scope is per community and viewer (`"disconnected"` when offline). Written on resize, pagehide and unmount | `useSidebarView.ts:181-226`, `src/shared/view-state.ts:44-62` |
| Shared width | The Settings and Me sidebars read the same saved width but cannot resize it | `src/app/SettingsSidebar.tsx:27`, `src/bundled/me/MeSidebar.tsx:22` |

**Resize handle** (`src/features/panels/ResizeHandle.tsx`,
`src/features/panels/ResizeHandle.module.css`). The handle is an `<hr>` placed after the
sidebar. Only the ready channel sidebar renders it (`ChannelSidebar.tsx:1388-1391`).
- Hit area: 16px wide (`space-4`), full height, `margin: 0 -8px` so it is centered on the
  divider and takes no layout space. `z-index: 4`, `cursor: col-resize`, transparent.
- Grip (`::after`): 3px x 56px pill, color `border-focus` (light `#000`, dark `#fff`),
  vertically centered, x at `50% - 0.5px` (over the divider's last pixel). It is hidden
  (opacity 0, `scaleY(40/56)`) until hover, focus-visible or drag. It then animates to
  opacity 1 and `scaleY(1)` over 120ms (opacity with `ease`; transform with
  `cubic-bezier(0.23,1,0.32,1)`).
- Tooltip (`::before`): "Drag to resize · Double-click to reset". Popover surface, 1px
  border, radius 12, `shadow-sm`, caption 12px, padding 6x8. It sits 2px to the right of
  the handle, appears after 600ms of hover, and stays hidden while dragging.
- Behavior: pointer drag (live; `html[data-sidebar-resizing]` forces the `col-resize`
  cursor everywhere and disables text selection). Arrow keys change the width by 16px
  (48px with Shift). Home = min, End = max, double-click = reset to 260. The width is
  rounded and clamped to 220..520.

**Collapse (desktop)**. `.shell-navigation[data-sidebar-collapsible]` is a one-column grid
that animates `grid-template-columns` between `1fr` and `0fr` over `--duration-settle`
(220ms) `ease-in-out`. It clips during the move and keeps the sidebar mounted
(`globals.css:274-298`). Reduced motion turns the transition off. Only the Messages,
Agents, Me and Settings pages can collapse. Other pages always show the sidebar on
desktop.

### 2.9 Main region and page padding

`AppShell.tsx:421-445`.
- `main#main-content`: `flex: 1`, `min-width: 0`, `overflow: hidden`, focus target for
  navigation.
- "Workspace" layout pages (all bundled primary pages, and Settings): content fills
  `h-full`. The page owns its padding.
- Document pages (layout not "workspace"): a scroll container with padding 8px horizontal,
  40px top, 32px bottom. At 640px and up: 16px horizontal, 56px top, 40px bottom. Content
  is centered with `max-width: 56rem` (896px, Tailwind `max-w-4xl`).

### 2.10 Companion dock (right panel)

`src/features/panels/PanelFrame.tsx`, `src/features/panels/Panels.module.css:26-70`,
`src/features/panels/usePanelSplit.tsx`.
- PanelFrame is a grid. With the dock open, columns are `minmax(0,1fr)` and
  `minmax(300px, var(--secondary-panel-width, 420px))`.
- Default dock width 420px. Minimum 300px. Maximum: available width - 320px.
- Dock left divider: 1px `border-standard`. A `ResizeHandle` with `direction=-1` sits on
  that divider (same visuals as 2.8; label "Resize main and secondary panels").
- At <=1000px (62.5rem) the dock overlays the page: absolute, right-aligned,
  `width: min(420px, 100%)`, `z-index: 2`, `box-shadow: elevation-dock` (shadow-sm), no
  resize handle. At <=650px (40.625rem) it is full width with no divider.
- Motion: opens with fade and slide over 180ms `cubic-bezier(0.23,1,0.32,1)`. Desktop
  travel is 12px. Overlays travel 100% of the width and exit over 120ms; desktop closes
  immediately. Keyboard-initiated opens and closes are instant. Reduced motion uses fade
  only.
- The Messages page has its own copy of this split (`Channels.module.css:16-28, 458-560`).
  See the Messages section.

### 2.11 Breakpoints

| Breakpoint | Effect | Source |
|---|---|---|
| `max-width: 650px` (JS `matchMedia` and CSS) | The navigation (sidebar) is hidden. The header toggle "Show navigation" opens it as a drawer over the content: `position: absolute; inset-y: 0; left: 0; z-index: 20`, fill `surface-panel`, `shadow-sm`, `max-width: calc(100% - 16px)`. The sidebar is fixed at 220px. Escape closes it, and any navigation closes it. The resize handle is hidden. Header pages collapse into More pages | `AppShell.tsx:83-113`, `globals.css:364-387`, `Channels.module.css:562-571` |
| `max-width: 43.75rem` (700px) | Header `padding-left` changes from 16 to 8 | `globals.css:354-362` |
| `min-width: 40rem` (640px, Tailwind `sm`) | Header `padding-right` 16 (8 below). Shell body gutters 16 (8 below). Document page padding increases | `globals.css:201-205, 300`, `AppShell.tsx:431` |
| `max-width: 62.5rem` (1000px) | Companion dock and the Messages secondary pane overlay the content | `Panels.module.css:52-70`, `Channels.module.css:521-541` |
| `max-width: 40.625rem` (650px) | Dock is full width | `Panels.module.css:71-76` |
| container `max-width: 40.6875rem` (651px) on the Messages board | Secondary pane overlays when the content area is narrow because the sidebar is wide | `Channels.module.css:542-560` |

### 2.12 Page navigation and page icons

`src/app/shell/presentation.ts`, `src/app/shell/PageNavigation.tsx`, `src/app/shell/PageIcon.tsx`.

- Order: Me, Messages, Inbox, Reminders, Bestie, Projects. Other pages follow by label
  (`localeCompare` "en"), then by full key.
- Placement: Me and Messages go to the topbar. Every other primary page is a **sidebar
  row** at the top of the channel sidebar (3.2).
- Icons:

| Page | Key | Tabler icon | Enabled by default |
|---|---|---|---|
| Me | `buzz.me/me` | (topbar, text only) | yes |
| Messages | `buzz.channels/channels` | `IconMessages` (search and other surfaces) | required |
| Inbox | `buzz.inbox/inbox` | `IconBell` | yes |
| Reminders | `buzz.reminders/reminders` | `IconAlarm`; trailing badge with due count in `text-navigation-unread`, weight 600 (`src/bundled/reminders/Reminders.module.css:59-62`) | yes |
| Bestie | `buzz.bestie/bestie` | custom Bestie mark | no |
| Projects | `buzz.projects/projects` | `IconFolder` | yes |
| Agents | `buzz.agents/agents` | `RobotIcon` (custom Soft & wide mark, no optical lift) | yes |
| Workflows | `buzz.workflows/workflows` | `IconBolt` | yes |
| Agents2 / unknown plugin | any | `IconBrowser`, or the plugin's image | yes |
| Settings | `settings` | `IconSettings` | n/a |

(Enabled-by-default: `src/bundled/index.ts:60-215`.) Sidebar page icons are 16px with
`strokeWidth 2.5`, inside an 18x18 box (`.shell-page-icon`, `globals.css:263-269`).

---

## 3. Channel sidebar

Sources: `src/features/channel-navigation/ChannelSidebar.tsx`,
`src/features/channel-navigation/SidebarFrame.tsx`,
`src/features/channel-navigation/ChannelSidebarDnd.tsx`,
`src/bundled/channels/Channels.module.css`, `src/bundled/channels/ChannelSidebarRow.tsx`
and `.module.css`, `src/bundled/channels/ChannelSidebarItem.tsx`,
`src/bundled/channels/SidebarSection.tsx`, `src/bundled/channels/UnreadBadge.tsx`,
`src/bundled/channels/SidebarUnread.tsx`, `src/bundled/channels/sidebar-sections.ts`.

### 3.1 Container and local tokens

DOM: `div.shell-sidebar` (width from storage; see 2.8) > nested `Panel as="aside"`
(flush, fill `surface-panel`) > `div.sidebar` > `SidebarUnread`'s `div.channelListFrame` >
`nav.channelList` (scroll container) > `div.channelListContent`.

`.sidebar` (`Channels.module.css:41-52`) overrides these tokens for everything inside it:

| Token | Value inside sidebar |
|---|---|
| `--type-weight-semibold` | **600** (unread rows use true semibold) |
| `--size-row` | 24px |
| `--interaction-selected` | `affordance-selected` (light `#e8e8e8`, dark `#333333`) |
| `--affordance-panel-hover` | `affordance-navigation-quiet` (light `#f7f7f7`, dark `#232323`) |
| `--sidebar-padding` | 8px |
| `--channel-row-radius` | 12px (declared, not used by rows) |
| `--sidenav-emoji-size` | 13px |

- `.sidebar`: flex column, `padding-inline: 8px 8px`, `overflow: hidden`.
- `.channelListFrame` (`:141-149`): `position: relative`, `width: calc(100% + 6px)` (moves
  the scrollbar 6px into the right padding), `min-height: 53px`, `flex: 1`.
- `.channelList` (`:150-168`): `overflow-y: auto`, `padding-block: 8px`,
  `scrollbar-gutter: stable`. Thin scrollbar, 5px WebKit width with a 2px left
  transparent border. The thumb is transparent until the list is hovered or focused, then
  `--scrollbar-thumb-quiet` (90% `border-standard`). Thumb fade: 90ms. On touch devices
  the thumb is always visible (`:186-205, 428-438`).
- Row fill insets for every row inside the list: `--channel-row-fill-start: 4px`,
  `--channel-row-fill-end: 10px` (6px plus the 4px fill-start). The highlight is 4px in
  from the left and 10px in from the right of the scroll content.
- `.channelListContent` (`:206-215`): flex column, **gap 2px**, `padding-bottom: 8px`.

Horizontal geometry at the default 260px width (x measured from the sidebar's left
edge):

| Element | x |
|---|---|
| Scroll content | 8 .. 252 (244px wide after the 5px scrollbar gutter) |
| Row highlight fill | 12 .. 242 (230px) |
| Row icon box (20px) | 16 .. 36 |
| Label start | 42 (6px gap after the icon) |
| Trailing badges end | 236 (16px from the content's right edge) |
| Section header text start | 16 |

### 3.2 Brand mark and page rows (top of the list)

`ChannelSidebar.tsx:148-164`, `Channels.module.css:72-91`.
- **Brand mark** row: height 24px, `padding-top` 4px, `padding-left` 6px. Glyph 20x15px,
  `currentColor` (text-standard), drawn as a CSS mask of `/public/buzz-mark.svg`, nudged
  1px right.
- **Destinations** (`.destinations`): flex column, gap 2px, `margin-bottom` 12px. Contains
  the sidebar-placed pages (2.12). Each row is a `NavigationItem` and uses the same row
  recipe as channel rows (3.3): 28px tall, 18px icon box, 16px icon with stroke 2.5. An
  optional trailing badge is in `text-metadata` color unless the plugin styles it.

### 3.3 Channel row

`NavigationItem` (`src/shared/design-system/ui/NavigationItem.tsx`, `components.css:887-925`)
with sidebar overrides (`Channels.module.css:93-133, 169-185`) and the row wrapper
(`ChannelSidebarRow.module.css`).

| Property | Value |
|---|---|
| Height | **28px** (min-height and height `space-6 + space-1`) |
| Wrapper (`.row`) | flex, `margin-bottom: 2px`. The highlight is painted by `::before`, inset 4px left and 10px right, radius 8px. Fill transition 90ms `cubic-bezier(0.23,1,0.32,1)` |
| Button padding | left 8px, right 16px (10px fill-end + 6px). Top/bottom 2px |
| Inner gap | 6px (`--space-row-gap` override) |
| Corner radius | 8px (`radius-row`) |
| Font | `text-body-sm`: 14px / 20px, weight 400, letter-spacing -0.0025em |
| Text color | `text-navigation` (light `#404040`, dark `#c7c7c7`) |
| Icon box | `.iconSpace` 20px wide x 24px tall, content centered |
| Channel icon | Tabler `IconHash` (public) or `IconLock` (private), 16px, default stroke 2, color inherits the row text (`src/features/channels/channel-icon.ts`) |
| Label | `FadingLabel`, single line, `text-overflow: clip`. When it overflows, a mask fades the last 20px to transparent. **No ellipsis** (`FadingLabel.module.css`) |
| Hover | highlight fill `affordance-navigation-quiet` (light `#f7f7f7`, dark `#232323`). Text and icons switch to `text-standard` |
| Selected | highlight fill `affordance-selected` (light `#e8e8e8`, dark `#333333`). Text `text-standard`. No accent bar, no bold |
| Focus-visible (keyboard) | 2px `text-standard` outline, 2px offset |
| Disabled | color `text-unavailable`, no hover fill |
| Context menu | right-click, ContextMenu key or Shift+F10: Move channel/conversation submenu (Starred, saved groups, Direct messages for DMs, separator, Create new…, Remove from <section>), Mute/Unmute (`IconBellOff`/`IconBell`), mark read/unread, Copy link, lifecycle actions (archive/leave), and "Remove from Messages" (`IconMinus`) for DMs. Menu icons 14px |
| Hover popover | A row with thread or agent activity opens `ChannelActivityPopover` after 250ms of hover, to the right (`ChannelActivityPopover.tsx:240-290`) |

**Unread styles** (`UnreadBadge.tsx`, `ChannelSidebarRow.module.css:115-118`,
`Channels.module.css:772-795`):

| State | Visual |
|---|---|
| Ordinary unread backlog (any observed unread in a DM; in a channel, more unread than attention-worthy messages; or manually marked unread) | Label color `text-navigation-unread` (light `#262626`, dark `#ffffff`), **weight 600**. The icon keeps the normal color |
| Priority (any DM unread, or a channel with mentions/attention) and no thread activity | A **6x6px dot**, circle, fill `affordance-prominent` (light `#000000`, dark `#ffffff`), pushed right (`margin-left: auto`) |
| Unread threads | The same 6px dot (`.threadActivityDot`, `cursor: help`, tooltip "N unread threads") |
| Mention count pill / numeric badge | **None.** Counts appear only in accessible labels ("N observed unread messages. Not an exact total.") |
| Agent working | Pill 26x15px, `radius-pill`, fill `affordance-subtle-hover` (light `#f1f1f2`, dark `#2e2e2e`). Three 3px `text-standard` dots pulse in sequence: 1.2s linear, delays 0/80/160ms, opacity 0.45 to 1, lift 2px. The pill stacks over the unread dot in the same grid cell (`Channels.module.css:854-912`) |
| Badge container | `.badges`: inline-flex, gap 8px, `padding-right` 4px. Hidden when empty (`ChannelSidebarRow.module.css:79-88, 151-153`) |

**Muted channels**: the sidebar has no muted styling (no dimming and no icon). Mute
only affects notifications (`src/features/notifications/messages.ts:137`) and the menu
label.

### 3.4 DM rows

`ChannelSidebarItem.tsx:174-230`, `Channels.module.css:914-972`, `components.css:392-478`,
`src/shared/design-system/styles/avatar-status.css`.

| Variant | Identity glyph |
|---|---|
| 1:1 DM | Avatar in a 22x22px box (`.dmAvatar`). **Circle** for humans, **squircle** (mask `avatar-squircle.svg`) for agents (`profile.isAgent`). Image is the profile picture through the media proxy ("small"). Fallback: uppercase initial on `affordance-subtle` with `text-subtle` |
| Group DM (more than one other participant) | 22x22px circle, fill `affordance-subtle`, `text-subtle`, `text-caption` 12px showing the participant count. Tooltip "N other participants" |
| No participants | `IconMessageCircle` 16px |
| Someone typing (humans only) | Replaces the identity: a 22x22 pill, fill `affordance-subtle`, with three 4px `text-subtle` dots pulsing (1.4s, 0/0.2/0.4s delays, opacity to 0.35) |
| Name accessory | The user's status emoji (`UserStatusDisplay compact`), emoji 13px, 8px after the name |

- The icon box is still 20px wide with `margin-right: 2px`. The 22px avatar overflows it
  by 2px.
- **Presence dot** (`components.css:448-478`): `width: 25%` of the avatar (5.5px on 22px;
  8px on 32px; 10px on 40px), `aspect-ratio: 1`, centered at `top: 85%; left: 85%`
  (translate -50%/-50%). Colors:

| Presence | Light | Dark |
|---|---|---|
| Online | ring `status-avatar-online-border` = green-11 `#218358`, inner fill (inset 1px) green-10 `#2b9a66` | ring `#3dd68c`, fill `#33b074` |
| Away | `status-away` amber-10 `#ffba18` (solid) | `#ffd60a` |
| Offline | `status-offline` neutral-8 `#808080` | neutral-10 `#a4a4a4` |
| Unknown | no dot | no dot |

- When a dot is shown, the avatar is masked with a **cutout**
  (`avatar-status-cutout.svg`, 40-unit viewBox: outer radius 20, cutout circle radius 8
  centered at (34,34)). This leaves a transparent gap ring around the dot. Squircle
  avatars use `avatar-status-squircle-cutout.svg`, and their dot is itself squircle-masked.

### 3.5 Section headers

`SidebarSection.tsx:118-307`, `Channels.module.css:292-438`.

| Property | Value |
|---|---|
| Element | `<details><summary>` with an `aria-label` equal to the title |
| Summary box | flex, `align-items: center`, gap 4px, min-height **32px**, padding `2px 72px 2px 8px` (the right padding reserves room for two 32px actions plus 8px), radius 12px |
| Text | Title in data casing (not uppercased): "Starred", the group name, "Channels", "Forums", "Direct messages". `text-body-sm` 14px/20px, weight **400**, color `text-unavailable` (light `#959595`, dark `#595959`). Single line with the fading mask (no ellipsis) |
| Group icon | Only for saved groups: a 16x16 box, emoji at 13px, or a custom community emoji image 13x13. `margin-right` 8px. The placeholder is a `border-standard` square while loading |
| Collapse chevron | **After** the title: Tabler `IconChevronDown`, 15px, stroke 2.5, centered in a 24x24 box, color `text-unavailable`. Rotated -90 degrees when collapsed, 160ms ease-out (none with reduced motion). Always visible |
| Hover on summary | No fill change. Only the actions appear |
| Focus-visible | 2px `border-focus` outline, inset -2px |
| Actions | Absolutely positioned at `right: 4px`, vertically centered, color `text-unavailable`. **Opacity 0** until the section's summary or actions are hovered or focused, or a popup is open. Always visible on touch (`hover: none`) |
| Action buttons, in order | 1. **More actions** `IconDots` 2. **New message** `IconPlus` (Direct messages only) 3. **Create channel** `IconPlus` (Channels and saved groups; disabled with tooltip "Channel creation unavailable" when not allowed). All are ghost IconButtons `size="compact"`, so 32x32, radius 10. The 15px / stroke 2.5 icons render at 16px because of the `data-size="sm"` SVG rule (`components.css:70-73`). Hover/pressed fill `affordance-navigation-quiet` |
| More-actions menu | Sort submenu (`IconArrowsDownUp`) with Recent (`IconStopwatch`) and A–Z (`IconLetterCase`); Collapse/Expand section (`IconChevronUp`/`IconChevronDown`); for groups, separator then "Remove section" (`IconTrash`, danger), which asks for confirmation in a dialog |
| Collapsed state | Content `hidden` and inert. Stored in `collapsed[]` in the persisted view state (2.8) |
| Spacing | 16px between sections (`.channelSection + .channelSection`). Rows sit directly under the header; rows are spaced only by their 2px bottom margin |

### 3.6 Section order and row ordering

`sidebar-sections.ts:56-116`.
1. **Starred** (icon "★" is used in the move menu, not in the header). Shown only when it
   has rows.
2. **Saved groups** in preference order. Always shown, even when empty.
3. **Channels**. Always shown. Contains non-DM, non-forum channels that are not starred
   and not assigned to a group.
4. **Forums**. Shown only when it has rows.
5. **Direct messages**. Always shown. Contains DMs that are not hidden, not starred and not
   grouped.

Archived channels are excluded. Hidden channels are excluded unless they are DMs. With
the Me page enabled, Me-placed conversations are excluded.

Sorting is per section (`preferences.sort[key]`, default `"alpha"`):
- `alpha`: case-insensitive name, then id.
- `recent`: `lastActivityAt` descending. Rows with an activity time come before rows
  without one. Ties fall back to alpha.

### 3.7 Drag and drop

`ChannelSidebarDnd.tsx`, `Channels.module.css:299-326`.
- A drag starts after 6px of pointer movement, so a click still selects. Collision uses
  `pointerWithin`.
- Draggable: non-DM channel rows in Starred, Channels or group sections. DMs and forums
  are not draggable. Drop targets: Starred, Channels and groups. A drop on the source
  section does nothing.
- The source row is dimmed to opacity 0.3 (`[data-channel-dragging]`).
- Drop target section: whole-section fill `affordance-navigation-quiet`, radius 8px.
- Drag overlay (portal, no drop animation): flex, gap 8px, padding 4px 8px, fill
  `surface-popover` (light `#fff`, dark `#282828`), 1px `border-standard`, radius 12px,
  `shadow-sm`, 14px text, `cursor: grabbing`. Content: channel icon 16px and the name
  (ellipsis).
- On drop the row moves optimistically. A failed save shows a notice in the footer (3.9).

### 3.8 Unread edge pills ("N unread" above/below)

`SidebarUnread.tsx`, `Channels.module.css:216-291`.
- When unread rows are scrolled out of view, a floating pill appears, centered
  horizontally. The "above" pill sits at `top: 20px`; the "below" pill sits at
  `bottom: 0`. Both are inside the list frame.
- The pill is a `Button variant="prominent" size="sm" shape="control"`: min-height 32px,
  padding 6px 16px, radius 12px, fill `affordance-prominent` (light black, dark white),
  text `text-inverse`, 14px/500, `shadow-sm`, max-width `100% - 8px`.
- Content: `IconArrowUp`/`IconArrowDown` (16px), then up to 3 DM avatars (20px, ringed by
  2px of the pill color, overlapping by -6px, the nearest on top) followed by "·", then
  "N unread" (ellipsis). Inner gap 6px.
- Entrance: opacity 0 to 1, `translateY(2px) scale(.97)` to identity, blur 2px to 0, over
  150ms. Exit: 120ms. Reduced motion: opacity only.
- Clicking scrolls the nearest unread row to the center (expanding a collapsed section
  first) and focuses it without opening it.

### 3.9 Empty, loading and footer states

- Before the relay is ready, the sidebar uses `.sidebarScroll` instead of the list frame,
  showing the brand and page rows, then a message in `.empty` (padding 32px 16px,
  `text-subtle` 14px):
  - "Connecting to your relay…"
  - "Choose a community to see channels."
  - the error text with a **Retry channels** button.
  
  (`ChannelSidebar.tsx:120-146`, `Channels.module.css:61-71, 447-454`.)
- In the list: "Loading your sidebar…" (status), "No channels yet.", or the list error.
- The error boundary shows "Channels couldn’t open." with Retry channels.
- **Footer notices** below the list, inside the frame (`.preferenceNotice`: padding 8px,
  caption 12px, `text-subtle`, `border-top` 1px `border-standard`):
  - "Offline · Showing saved conversations." with Retry connection
  - sort save failures
  - move save failures (Retry move / Dismiss)
  - "Saved groups and stars aren’t supported by this host yet."
  - toast-style warnings for mute failures, preference refresh and recent-activity refresh
  
  (`ChannelSidebar.tsx:1218-1366`).
- There is no permanent footer (no user card or settings button). Settings is reached from
  the profile menu.

---

## 4. Channel page

All source paths are relative to `/home/ben/projects/buzz-app`. The root font size is never changed, so 1rem = 16px. Only the `--type-*` sizes follow the user's font-size preference: `:root[data-font-size="smaller"]` multiplies them by 0.9 and `"larger"` by 1.1 (`src/shared/design-system/styles/typography.css:16-35`). Spacing, radii and control sizes do not scale. All px values below are at the default scale.

### 4.0 Tokens used in this section (resolved)

Sources: `src/shared/design-system/styles/tokens.css` (light `:root` at 270/520/671, dark `:root[data-color-mode="dark"]` at 399/592/809), `src/shared/styles/tokens.css` (legacy aliases), and `src/shared/design-system/styles/typography.css`.

| Token | Value |
|---|---|
| `--space-half / -1 / -1h / -2 / -3 / -4 / -5 / -6 / -8 / -16` | 2 / 4 / 6 / 8 / 12 / 16 / 20 / 24 / 32 / 64 px |
| `--space-panel-inset` | 24px |
| `--size-control-sm / --size-control / --size-control-lg` | 32 / 40 / 52px |
| `--size-row` | 32px |
| `--size-panel-header` | 40px |
| `--radius-chip` | 4px |
| `--radius-row` | 8px |
| `--radius-icon-button` | 10px |
| `--radius-control` | 12px |
| `--radius-container` | 16px |
| `--radius-panel` | 20px |
| `--radius-capsule` | 26px |
| `--radius-pill` | 9999px |
| `--shadow-xs` | `0 1px 2px rgba(0,0,0,.05)` |
| `--shadow-sm` | `0 0 2px rgba(0,0,0,.1), 0 4px 8px -2px rgba(0,0,0,.06)` |
| `--duration-fast / -state / -panel-enter / -settle` | 120 / 150 / 180 / 220 ms |
| `--easing-enter` | `cubic-bezier(.23,1,.32,1)` |
| `--easing-settle` | `cubic-bezier(.645,.045,.355,1)` |
| `--blur-sm / md / lg` | 8 / 16 / 32px |
| font-sans | "Inter Variable", Inter, system-ui |
| font-mono | "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace |

| Type role | size / line-height / letter-spacing / weight |
|---|---|
| `text-caption` | 12 / 16px / 0.0133em / 400 |
| `text-body-sm` (message body, byline) | 14 / 20px / -0.0025em / 400 |
| `text-body` (composer input) | 14 / 20px / -0.005em / 400 |
| `text-label-sm` (buttons) | 14 / 20px / 0.0025em / 500 |
| `text-label` | 16 / 24px / 0 / 500 |
| `text-body-lg` | 20 / 28px / -0.01em / 400 |
| `text-heading` | 24 / 26.4px / -0.0075em / 500 |
| `text-mono-sm` | 12 / 16px / 0.03em / 400 (JetBrains Mono, tabular-nums) |

| Colour role | Light | Dark |
|---|---|---|
| `--surface-panel` (timeline background) | #ffffff | #1a1a1a |
| `--surface-inset` | #f5f5f6 | #101010 |
| `--surface-popover` | #ffffff | #282828 |
| `--surface-elevated-glass` (popups, with 8px backdrop blur) | rgb(255 255 255 / 90%) | rgb(40 40 40 / 90%) |
| `--text-standard` | #0f0f0f | #ffffff |
| `--text-subtle` (= `--text-secondary`) | #525252 | #c7c7c7 |
| `--text-metadata` (= `--text-tertiary`) | #5f5f5f | #a4a4a4 |
| `--text-unavailable` | #959595 | #595959 |
| `--text-link` | #0b5fa8 | #83c4ff |
| `--affordance-link-hover` | #e6f4fe | #0d2847 |
| `--text-warning` | #4f3422 | #ffe7b3 |
| `--border-standard` | #e8e8e8 | #232323 |
| `--border-prominent` | #808080 | #737373 |
| `--border-focus` | #000000 | #ffffff |
| `--border-image` | oklch(0 0 0 / .1) | oklch(1 0 0 / .1) |
| `--affordance-subtle` (legacy `--surface-control`) | #f5f5f6 | #232323 |
| `--affordance-subtle-hover` (legacy `--surface-hover`) | #f1f1f2 | #2e2e2e |
| `--affordance-subtle-pressed` | #dadada | #333333 |
| `--affordance-selected` | #e8e8e8 | #333333 |
| `--affordance-popover-selected` | #f5f5f6 | #404040 |
| `--affordance-prominent` (legacy `--primary`) | #000000 | #ffffff |
| `--affordance-prominent-hover` | #191919 | #c7c7c7 |
| `--text-inverse` | #ffffff | #000000 |
| `--bg-glass-secondary` (glass-4, with 32px blur) | #ffffffc7 | #1c1c1cc7 |
| `--status-avatar-online` / ring `--status-avatar-online-border` | #2b9a66 / #218358 | #33b074 / #3dd68c |
| `--status-away` | #ffba18 | #ffd60a |
| `--status-offline` | #808080 | #a4a4a4 |

Buttons (`src/shared/design-system/styles/components.css:9-300`, `ui/Button.tsx`, `ui/IconButton.tsx`):

- **IconButton sizes:** `md` is 40×40 with a 24px glyph. `sm`, `toolbar` and `compact` are 32×32 with a 16px glyph, and CSS forces this glyph size even when the icon passes another size. `xs` is 20×20 with a 12px glyph.
- **Corners:** the default shape uses `--radius-icon-button` (10px). `shape="round"` uses a pill.
- **Ghost buttons:** transparent at rest. Hover is `--interaction-hover`, which is `--affordance-subtle-hover` on panels and `--affordance-floating-control-hover` inside popups (light #f1f1f2, dark #595959). Pressed is `--interaction-pressed`.
- **Disabled:** text `--text-unavailable`. A disabled ghost button stays transparent.
- **Text buttons:** `sm` has min-height 32px, padding 6×16 and the label-sm role. `md` has min-height 40px and padding 8×24. Both use the capsule radius (26px), or 12px with `shape="control"`.

Tabler icons are referenced by Tabler kebab name. The mapping from Buzz alias to Tabler name is in `src/shared/design-system/icons/index.ts`.

---

### 4.1 Page layout

Source: `src/bundled/channels/ChannelsPage.tsx:1203-1676` and `src/bundled/channels/Channels.module.css:16-28, 458-583`.

- **Workspace grid (`.board`):** one column, `minmax(0,1fr)`. When the right dock (thread, settings or tab pane) is open, the columns are `minmax(320px, 1.1fr) minmax(316px, var(--secondary-panel-width, 1fr))`.
- **Conversation column:** a `Panel` (`<article aria-label="Conversation">`) holding a flex column. From top to bottom:
  1. Header (§4.2).
  2. Live-status toast (only on error).
  3. Timeline (`flex:1`, §4.3).
  4. Read-only join notice.
  5. Composer context (typing indicator, §4.10).
  6. Composer (§4.8).
- **Panel surface:** border 1px `--border-standard`, radius 20px, background `--surface-panel`, `--shadow-xs`. The panel root font is body-sm 14px/20px in `--text-standard` (`Channels.module.css:4-12`).
- **Empty or placeholder state:** `.empty` has padding 32px 16px and uses 14px `--text-subtle`. Possible strings:
  - "Select a channel to read it."
  - "Checking conversation access…"
  - "Loading messages…"
  - An error message followed by a "Retry messages" button.
  - "No messages yet." (timeline, `ChannelTimeline.tsx:835-837`).

---

### 4.2 Channel header

Sources: `ChannelsPage.tsx:1316-1430`, `src/shared/design-system/ui/PanelHeader.tsx`, `ui/Tabs.tsx:58-135`, and `components.css:744-800` (panel-header), `887-934` (navigation-item), `988-1060` (navigation tabs).

**Frame**

| Property | Value |
|---|---|
| Element | `<header class="panel-header">` |
| Height | min 40px (`--size-panel-header`); flex row with wrap, `align-items:center`, `justify-content:space-between` |
| Padding | 0 top and bottom, 4px left and right |
| Gap | 8px between title and actions |
| Bottom rule | `::after` hairline, 1px `--border-standard`, inset 4px from each side |

**Title**

The title is not an `<h1>`. It is a single, non-selectable navigation tab (`Tabs variant="navigation" showSelection={false}`).

| Property | Value |
|---|---|
| Tab list | height 40px, padding-top 4px, `align-items:flex-start`, gap 4px |
| Tab width | fixed 192px (`flex: 0 0 12rem`), max-width 100% |
| Tab item | min-height 32px; padding 4px 8px; radius 10px; gap 8px; 14px body-sm, `--text-standard`; background transparent with no hover fill (`[data-selection-hidden]`); cursor default |
| Icon box | 16×16 (`.buzz-tabs-icon`). Public channel uses `hash`, private uses `lock` (`src/features/channels/channel-icon.ts`), DM uses `message-circle`. Glyph is 16px. |
| Label | channel name without a "#" prefix, single line with ellipsis. Before a channel is selected the label reads "Channels". |

Session channels use a different heading (`SessionHeading`), which is out of scope here.

**Topic, description, member count and avatars:** none of these appear in the header. They live in the "View channel details" settings panel.

**Actions** (`.panel-header-actions`: flex, gap 4px, `margin-left:auto`), in order:

| # | Control | Tabler icon | Size | Notes |
|---|---|---|---|---|
| 1 | "Channel members" | `users` | 32×32 (sm), 16px glyph | Opens the members dialog |
| 2 | Plugin channel launchers (`.channelLaunchers`: flex, gap 6px, `margin-left:auto`) | Terminal: `terminal-2`. Todos: `list-check`. | 32×32 | Terminal is desktop-only; the Todos plugin is off by default (`src/bundled/index.ts`) |
| 3 | "Channel actions" menu | `dots` | 32×32 | Stays unfilled while its menu is open (`data-highlight-expanded="false"`) |
| 4 | "Toggle tab pane" | `layout-sidebar-right` | 32×32 | Shows the `--affordance-subtle` fill while the pane is open (`Channels.module.css:52-59`) |

The channel actions menu (`ChannelHeaderMenu.tsx:84-122`) is `align="end"`. Its items, in order:

1. "View channel details" (`info-circle`)
2. "View canvas" (`file-text`)
3. Plugin panel items
4. Separator
5. Read and lifecycle actions

---

### 4.3 Timeline container

Sources: `src/features/messages/ChannelTimeline.tsx:685-839` and `Messages.module.css:21-76, 2261-2305`.

| Element | Spec |
|---|---|
| Scroller (`section.feed`) | `flex:1`; `overflow-y:auto`; padding 0 24px, bottom 2px; thin scrollbar with stable gutter; background inherits the panel |
| Viewport ≤650px (40.625rem) | padding-inline 12px (`Messages.module.css:504-511`) |
| List | Virtualized `<ol>` with no list style. Rows are bottom-anchored: the view follows new messages while within 80px of the bottom. |
| Top edge (`.edge`) | Height 56px, centred flex, gap 8px, 12px caption, `--text-subtle`. Contents depend on state (see list below). |
| End spacer (`.timelineEnd`) | 12px empty block after the last row |

Top edge contents:

- When older history exists: a "Load older messages" button (subtle variant `md`, 40px tall, capsule radius, `--affordance-subtle` fill). While loading it reads "Loading older…"; after an error it reads "Retry older messages".
- "History window limit reached" when the window cap is hit.

Older history also loads automatically when the reader scrolls within `max(3000px, 4 × viewport)` of the top.

**Jump-to-latest button** (`JumpToLatestButton.tsx`, CSS 2261-2305)

- **Visibility:** shown when the distance from the bottom is 80px or more.
- **Position:** `position:sticky; top: calc(100% - 32px)` with a zero-height wrapper, so the pill's bottom sits on the bottom of the viewport, centred horizontally. When an agent-activity pill is present it moves up an extra 48px (`2 × --space-6`).
- **Surface:** `glass-secondary` (bg `--bg-glass-secondary`, 32px backdrop blur, `--shadow-sm`, inset rim `0 1px 0 #ffffff73`, `0 -1px 0 #ffffff1f`), radius 12px.
- **Button:** ghost `sm` with control shape: 32px tall, padding 6px 16px, 14px/500, gap 8px. Contents are an `arrow-down` icon (16px) and a label.
- **Label:** "Jump to latest", or "N new message" / "N new messages" when rows arrived while scrolled up.
- **Show:** opacity 0→1, `translateY(2px) scale(.97)`→none, `blur(2px)`→0, over 150ms.
- **Hide:** the same properties reversed over 120ms.
- **Reduced motion:** opacity only.

**Unread or "new messages" divider:** none. The timeline draws no unread line. The only cue is the jump button's "N new messages" count.

---

### 4.4 Message rows and grouping

Sources: `MessageRow.tsx:383-895`, `message-grouping.ts`, `MessageTimestamp.tsx`, `src/shared/datetime.ts`, and `Messages.module.css:94-161, 1636-1681, 1798-1833, 2254-2256`.

**Grouping rule** (`continuesMessageGroup`)

A row becomes a continuation of the previous row only if all of these hold:

- Same channel and same `authorId`.
- `current.createdAt ≥ previous.createdAt`, and the gap is **≤ 5 minutes** (300s).
- Same local calendar day.
- Neither row is a membership (system) row.
- Neither row has a `workflowOwnerId`.
- Neither row was "sent from thread".

A day change always inserts a day divider, and the row after it starts a new group.

**Row geometry**

| | Group head (`layout="timeline"`) | Continuation |
|---|---|---|
| Container (`.message`) | flex row, gap 12px, padding **14px top / 2px bottom** | padding 2px top / 2px bottom, gap 12px (timeline) |
| Left column | Avatar 40×40 (size `large`). When the profile is openable it becomes a 40px round ghost IconButton holding a fill avatar. | Gutter 40px wide (`flex-basis: --size-control`), content centred and top-aligned, holding the hover-only time |
| Body | `.messageBody`: flex column, `flex:1`, `min-width:0` | Same; byline is screen-reader only |
| Text top margin | 4px | 0 |

- **Spacing between groups:** 2px + 14px = **16px**.
- **Spacing between continuation rows:** 2px + 2px = 4px.
- **Hover background:** none. Rows have no hover or selected background.

**Avatar**

- Size 40px.
- People use a circle. Agents (agent envelope, known agent pubkey, or `profile.isAgent`) use a squircle mask (`avatar-squircle.svg`).
- Fallback is the first grapheme of the name, upper-cased, in `--text-subtle` on `--affordance-subtle`.
- Images use `object-fit:cover` and fade in once loaded.
- Agents can show a presence dot: 25% of the avatar size, centred at (85%, 85%), with a cut-out mask behind it. Online is an `--status-avatar-online` fill inside a 1px `--status-avatar-online-border` ring; away is `--status-away`; offline is `--status-offline`.
- Agents can also show a "thinking" badge: a green pill with three pulsing dots, `src/shared/design-system/ui/agent-thinking/thinking.css`.

**Workflow messages**

- Avatar: squircle with a `bolt` icon (20px).
- Byline: **"Workflow"** (500), then `· owned by {owner}` in `--text-subtle` (the owner name is an `xs` link button), then an `info-circle` xs (20px) IconButton that opens a "Workflow message" popover.

**Byline** (`.byline`)

- Flex row with wrap, `align-items:baseline`, gap 8px, 14px body-sm.
- Author `<strong>` uses weight 500, colour `--text-standard`. The name is the profile name, or the first 10 characters of the pubkey.
- `<time>` is 12px caption in `--text-subtle`, followed by any plugin markers. For example, the reminders plugin adds an `alarm` icon (14px).

There are **no** bot, "APP" or role text badges; agents are distinguished only by the squircle avatar.

**Timestamp formats** (`Intl.DateTimeFormat` with the default locale; en-US shown)

| Where | Format |
|---|---|
| Byline (`formatItemTimestamp(withTime:true)`) | Today: "2:34 PM". Yesterday: "Yesterday at 2:34 PM". 2–6 days ago: "Monday at 2:34 PM". Earlier this year: "Sat, Jun 20 at 2:34 PM". Earlier years: "Jun 20, 2025 at 2:34 PM". |
| Continuation gutter (`compact`) | `{hour:numeric, minute:2-digit}` with the day period removed, e.g. "2:34". 12px caption, line-height 20px (matches the body line), tabular-nums, `--text-subtle`, background `--surface-panel`. |
| Continuation time visibility | Pointer devices: opacity 0, switching to 1 with no transition on row hover, focus-visible, or when the action bar is revealed |
| Tooltip on any time | Full `dateStyle:full, timeStyle:long`, e.g. "Friday, October 2, 2026 at 3:05:09 PM EDT", 500ms delay, cursor default |
| Day divider (`formatDayGroupLabel`) | "Today", "Yesterday", weekday ("Monday") for 2–6 days ago, "Saturday, June 20" for earlier this year, "June 20, 2025" for earlier years |

**Day divider** (`.day`, CSS 77-93 and 615-617)

- Flex row with margin 12px 0. A 1px `--border-standard` line on each side (`flex:1`).
- Label is 12px caption in `--text-standard`, padding 6px 12px.
- Rendered inside the row wrapper above `.message`, so the head row's 14px top padding follows it.

**Body text** (`.text`)

- 14px/20px, letter-spacing -0.0025em, `overflow-wrap:anywhere`, `text-wrap: pretty`.
- Markdown is described in §4.6.
- Content over the Markdown size or depth limits renders as plain `pre-wrap` text (`.plainText`).

**Other in-body lines**

| Line | Spec |
|---|---|
| Sent from thread | "Sent from " plus a message link "Thread — {excerpt}" (or "Thread"). 12px caption, `--text-secondary`, margin-block 4px (CSS 2307-2311). Appears above the body. |
| Edited marker | **Not rendered.** An edited message shows only the replacement body (confirmed by the gallery fixture `tests/fixtures/message-gallery/examples.ts`, "Edited content"). |
| Delivery (`DeliveryNotice.tsx`, `delivery.ts`, CSS 604-614) | Shows nothing during the 10s grace period, so pending messages look identical to sent ones. After 10s, "Couldn’t send this message." for a failed send, or "Delivery not yet confirmed." with optional error text. 12px caption in `--text-warning`, flex wrap, gap 8px, margin-top 6px. Failed or unknown messages add a "Retry" ghost `sm` button. |
| Reaction delivery error | "Couldn’t update reaction." or "Reaction delivery not confirmed.", plus a "Retry reaction" ghost `sm` button |

**System and membership rows** (`MembershipRow.tsx`, `membership-rows.ts`, CSS 1483-1546)

- **Layout:** centred flex row, gap 12px, padding 8px top / 16px bottom, 14px body-sm in `--text-subtle`.
- **Avatars:** up to 3, each 28×28 with a 2px `--surface-panel` border, overlapping by -6px. Overflow shows a "+N" chip: 24px circle on `--affordance-selected`, 12px caption.
- **Grouping:** consecutive membership events on the same day, less than 1 hour apart, merge into one row.
- **Text patterns:**
  - "{A} joined"
  - "{A}, along with {B}, {C} and N others, left the channel"
  - "{A} joined and left the channel"
  - "{A} was added by {X}"
  - "{A} was removed by {X}"
  - "You were added by {X}"

---

### 4.5 Hover action bar, reactions and thread summary

**Action bar** (`MessageActionBar.tsx`, `useFloatingActionBar.ts`, CSS 1579-1681)

| Property | Value |
|---|---|
| Surface | `popover-surface`: border 1px `--border-standard`, radius **12px**, fill `--surface-elevated-glass` with 8px backdrop blur, `--shadow-sm`, z-index 1100 |
| Padding / gap | 2px / 2px. Bar height = 32 + 2×2 + 2×1 = **38px**. |
| Position (pointer devices) | A zero-height slot at `top: calc(100% + 4px)` of the byline header, right edge = message body right edge. The bar is translated up by its own height, so **its bottom edge sits 4px below the byline and its right edge aligns with the row's right edge**. Continuations anchor the slot at `top:0`, so the bar floats above the row. Rendered as a fixed popover and hidden if the anchor scrolls above the timeline viewport. An invisible 8px hover bridge sits above the hovered row. |
| Show / hide | Shown on row hover, keyboard focus inside the row, or while a menu or picker is open. No fade: opacity switches between 0 and 1. |
| Touch / coarse pointer | Shown inline and always visible at the end of the header (`order:1`, `align-self:flex-end`, `margin-left:auto`) |

Buttons, left to right (all 32×32 ghost, 16px glyph, radius 10px unless noted):

1. Quick reactions: the 3 most-used emoji, defaulting to 👍 ❤️ 😂 (`quick-reactions.ts`).
   - Glyph is the inherited 14px emoji.
   - Hover lifts the glyph `translateY(-1px) scale(1.12)`; press scales it to 0.96 (120ms `--easing-enter`).
   - Hidden when the row is ≤260px wide.
2. "Add reaction": `mood-plus`, round (pill) shape.
3. "Reply": `message-circle`. Opens or replies in the thread; omitted when not available.
4. "Copy link": `link`.
5. "More message actions": `dots`. Menu `align="end"`, items in order:
   1. "Mark unread" / "Mark read" (`mail` / `mail-opened`)
   2. "Copy message" (`copy`)
   3. "Send to channel" (custom hash-arrow-in icon; only for your own thread replies)
   4. "Edit message" (`pencil`)
   5. "Delete message" (`trash`)
   6. "Follow thread" / "Unfollow thread" (`bell` / `bell-off`)
   7. Plugin actions, such as "Remind me" (`alarm`)
   8. "Report" (`flag`)

Copy actions show a success toast: "Link copied" or "Message copied".

**Reactions** (`MessageReactions.tsx`, CSS 170-373)

| Element | Spec |
|---|---|
| Row | Flex wrap, `align-items:center`, gap 6px, margin-top 6px. Takes no space when empty. |
| Pill | Inline-flex, centred, gap 6px. Min-width 48px, max-width min(160px, 100%), min-height **28px**, padding 0 8px, radius pill, border 1px `--border-standard`, fill `--affordance-subtle`, text `--text-standard`. Hover fill `--affordance-subtle-hover`. |
| Pill, mine (`aria-pressed`) | Border `color-mix(--affordance-prominent 40%, --border-standard)` (light ≈ #8b8b8b, dark ≈ #7b7b7b). Fill `color-mix(--affordance-prominent 12%, --surface-panel)` (light ≈ #e0e0e0, dark ≈ #353535). Text `--affordance-prominent` (#000 / #fff). |
| Emoji | Native emoji: 14px, line-height 18px, min-width 18px, max-width 96px. Custom image: 14×14, `object-fit:contain`. Failed custom emoji: `:shortcode:` text at 12px. |
| Count | 12px, line-height 14px, tabular-nums. Changes roll vertically over 260ms with `cubic-bezier(.23,1,.32,1)`. |
| Add-reaction button (after the pills, only when at least one reaction exists) | 28×28 round. Transparent border and fill at rest, glyph `--text-secondary`, `mood-plus` 16px. Hover, focus or open: border `--border-standard`, fill `--affordance-subtle`, glyph `--text-standard`. |
| Hover preview | Preview card on the top side, 100ms open delay; instant while moving along the row. Slides ±12px with scale .98 over 160ms. Contents: emoji plus its name (12px caption in `--text-tertiary`), then the names, label-sm, max-width 224px, e.g. "You, Sam Rivera". |

**Thread summary** (`MessageRow.tsx:842-891`, `ReplySummary.tsx`, CSS 665-675, 618-664, 727-750)

- **Element:** a ghost `sm` button, aligned flex-start, margin-left -4px, margin-top 4px.
- **Box:** padding 4px, right padding 6px, min-height 32px, radius 10px, gap 8px.
- **Type:** 12px caption, effective weight 400, `--text-standard`.
- **Hover:** fill `--affordance-subtle-hover`.
- **Contents, in order:**
  1. Up to 3 participant avatars. Each is 24×24 with a 1.5px `--surface-panel` ring (inner padding) and a -4px overlap; circle for people, squircle for agents. Overflow is a 24px "+N" circle on `--affordance-selected`.
  2. "N reply" / "N replies".
  3. Unread dot: 8px circle in `--text-standard`.
  4. Working badge (when an agent is active in the thread): a 26×15 pill on `--affordance-subtle-hover` holding three 3px dots that pulse in sequence (1.2s, 80ms stagger, lift -2px, opacity .45↔1).
  5. "View thread" text when there are no replies yet.
- **Last-reply time:** not shown.

---

### 4.6 Markdown rendering

Sources: `MessageMarkdown.tsx` (react-markdown with remark-gfm and remark-breaks, raw HTML skipped) and `Messages.module.css:1294-1462, 1847-1933`.

- **Line breaks:** a single newline is a hard line break (remark-breaks).
- **Syntax highlighting:** none (no highlighter is installed).
- **Copy button on code blocks:** none.

| Element | Spec |
|---|---|
| Block spacing | `p, ul, ol, blockquote, pre, table, hr` have margin 12px 0. First child has margin-top 0; last child has margin-bottom 0. |
| h1 | 24px / 1.1 / -0.0075em / 500 |
| h2 | 20px / 28px / -0.01em / 500 |
| h3 | 16px / 24px / 500 |
| h4–h6 | 14px / 20px / 500 |
| Heading margins and colour | margin 16px 0 6px, `--text-standard` |
| Bold | weight 500, the max weight; Inter is never synthesized |
| Strikethrough | GFM `~~text~~` |
| Lists | padding-left 24px, disc or decimal markers. `li + li` margin-top 4px; nested blocks inside `li` have margin 4px 0. |
| Task list | checkbox 1em×1em, margin-right 6px, bottom 2px, `vertical-align:middle`, `accent-color: --affordance-prominent` |
| Blockquote | padding 2px 0 2px 12px, border-left **1px** `--border-prominent`, text `--text-subtle` |
| Inline code | JetBrains Mono 12px/16px, 0.03em; padding 2px 6px; radius 4px; fill `--affordance-subtle` |
| Code block (`pre`) | border 1px `--border-standard`, radius 8px, fill `--surface-panel`, padding 12px. Scrolls horizontally with a thin scrollbar; inner code is `white-space:pre` and `width:max-content`. |
| Table | `display:block`, `width:max-content`, max-width 100%, horizontal scroll, `nowrap`. Border 1px `--border-standard`, radius 12px. Cells: padding 4px 8px, top-aligned, left-aligned, 1px right and bottom borders, with the last column and row unbordered. `th` uses fill `--affordance-subtle` and weight 500. |
| hr | 1px top border in `--border-standard` |
| Markdown image `![alt](…)` | Not loaded. Rendered as the alt text in italics, `--text-subtle`. |
| Links without a renderer | `--text-standard`, 1px underline at 2px offset; 2px underline on hover |

**Links** (`src/bundled/links/InlineLink.tsx`, `src/shared/InlineReference.module.css`)

- Every http(s) or `buzz://` link renders as an icon plus a label in `--text-link`.
- No underline. Padding 2px with a -2px inline margin, radius 4px; hover fill `--affordance-link-hover`.
- **Icon:** 1em, `vertical-align:-0.125em`, 3px gap after it.

| Link host | Tabler icon |
|---|---|
| Generic web | `world` |
| github.com | `brand-github` |
| drive.google.com | `brand-google-drive` |
| Google Docs / Sheets / Slides | `file-text` / `table` / `presentation` |
| figma.com | `brand-figma` |
| notion.so | `brand-notion` |
| slack.com | `brand-slack` |
| dropbox.com | `brand-dropbox` |
| OneDrive | custom mark |
| gitlab.com | `brand-gitlab` |
| youtube.com | `brand-youtube` |
| loom.com | `video` |
| zoom.us | `video-plus` |
| Teams | `brand-teams` |
| Buzz message | `message-circle` |
| Buzz thread | `messages` |

- **Raw URLs:** truncated to 44 characters plus "…".
- **Link previews and unfurls:** none inline; hovering a link shows a preview card.

**Mentions in messages** (`MessageMarkdown.tsx:502-542`, `ReferenceText.tsx:150-180`)

- Shown as link-coloured text, **not a filled chip**: `--text-link`, radius 4px, padding 2px, hover fill `--affordance-link-hover`.
- **Icon:** `at` (people, no gap before the name) or `RobotIcon` (agents, 3px gap), sized 1em.
- **Label:** the resolved display name without "@", since the icon stands in for the sigil.
- **Self-mention:** there is **no** highlight of the mention or the whole row.

**Channel references:** `hash` icon (`lock` for private channels) plus the channel name without "#", in link style. An unreadable private channel shows "Private channel".

**Custom emoji:** inline image 22×22 (`--inline-emoji-size` falls back to 1.375rem), `object-fit:contain`, `vertical-align:middle` (`src/bundled/emoji/Emoji.module.css:3-9`).

**Jumbo emoji** (`emoji-size.ts`, CSS 155-161)

- **Rule:** when the whole message is emoji only (Unicode pictographs, flags, keycaps and custom `:shortcodes:`, any count, whitespace allowed), the paragraph uses font-size and line-height **42px** (`--size-emoji-display`, 2.625rem), min-height 48px (`--size-emoji-line`) and margin-top 4px.
- Custom emoji in this mode are 42×42 with `vertical-align:top`.

**Spoilers** (`||text||`)

- The span is inline-block, radius 4px, padding-inline 2px.
- Hidden text sits under a mask of three tiled radial dot fields in `currentColor` that twinkle: opacity .35↔.9 with ±1px drift, over 0.65s and 0.9s alternate.
- Hover or revealed: fill `--affordance-subtle`.
- Click toggles between "Reveal spoiler" and "Hide spoiler".

---

### 4.7 Attachments

Sources: `MessageRow.tsx:653-795`, `AttachmentImage.tsx`, `FileAttachment.tsx`, `MediaAttachment.tsx`, `media-corners.ts`, and `Messages.module.css:385-466, 793-980, 1972-1979, 2206-2232`.

Attachments keep their send order. Only adjacent images share a strip, and only adjacent files share a file group.

| Kind | Spec |
|---|---|
| Image group | `.imageGroup` margin-top 8px. Its `.imageStrip` is a horizontal flex row with gap 8px, padding 4px and `overflow-x:auto`. |
| Image tile (timeline always uses thumbnail mode) | **72×72** square, `object-fit:cover`. Smooth corners with a 16px radius (60% smoothing: straight-edge inset 1.6r, clip-path in `mediaCornerPath`). 1px outline in `--border-image`. Placeholder: `--surface-inset` plus a blurhash canvas. Cursor zoom-in. Click opens the media review viewer. |
| Image count | When there are more than 1: "N images" below the strip, 12px caption in `--text-subtle`, margin-top 4px |
| Image unavailable | 72×72 tile, radius 8px, fill `--affordance-subtle-hover`, text "Image unavailable" as a 12px caption in `--text-subtle` |
| Non-thumbnail image (other surfaces) | Width min(360px, actual width, 320 × width/height), using the source aspect ratio; fallback ratio 360/320 |
| Video | Width min(384px, 100%, ratio × 180px), max-height 256px, smooth 16px corners. Centred play button. Expand button (`arrows-maximize`, 32px media-glass) at top-right with an 8px inset; hidden until hover. Time chip at bottom-left with a 12px inset: padding 4px 8px, pill, `--surface-inverse` fill, 12px tabular. |
| File card (`.fileAttachment`) | Inline-flex, gap 8px, max-width min(384px, 100%), padding 8px 12px, border 1px `--border-standard`, radius 12px, fill `--affordance-subtle`, hover `--affordance-subtle-hover`. Contents: `file-text` icon (24px); name (14px/500, ellipsis); meta in 12px `--text-subtle` ("12 KB", "Download file", "Open file" or "File unavailable"); `download` icon (20px) for downloadable files. Grouped files wrap with an 8px gap and margin-top 8px. |
| Audio | Width min(384px, 100%), margin-top 8px, padding 4px 8px, border 1px `--border-standard`, radius 32px, fill `--surface-inset`, 32px seek bar. Voice notes use the voice-notes plugin's card instead. |

---

### 4.8 Composer

Sources: `MessageComposer.tsx:1263-1830`, `Messages.module.css:468-554, 752-792, 1956-1960`, `EditableInput.module.css`, `ComposerFormattingTools.tsx` and `.module.css`, `ComposerAttachments.tsx` and `.module.css`, `src/features/conversation/ComposerTools.tsx`.

**Shell** (`form.composer`)

| Property | Value |
|---|---|
| Margin | 0 16px 16px. At ≤650px: 0 12px 12px. |
| Border / radius / shadow / fill | 1px `--border-standard` / **20px** / `--shadow-xs` / `--surface-panel` |
| Padding | **12px 6px 6px**. With attachments: padding-top 6px and gap 8px. |
| Layout | Flex column, gap 16px (`--composer-gap`), from top to bottom: completions portal, content, reply context, actions row |
| Typography | 14px/20px (text-body), `--text-standard` |
| Focus | **No focus ring or border change on the shell.** Keyboard navigation draws a 2px `--border-focus` outline with 2px offset on the editor. |
| File drag over | Border `--border-focus`, fill `--surface-inset`, plus the status text "Drop files to attach" |
| Read-only relay | The form is replaced by a footer (same shell) with the text "This relay connection supports reading only." |

**Content and input**

- **Content area** (`.composerContent`): column, gap 8px, padding-top 4px, padding-left 4px.
- **Editor** (`EditableInput.module.css:1-29`): min-height **24px**, max-height **192px**, then scrolls. `pre-wrap`; caret `--text-standard`. Holds up to 16,000 characters.
- **Placeholder:** single line with ellipsis, colour `--text-metadata`.
  - Channel: **"Message #{channelName}"**, for example "Message #general".
  - Thread: **"Reply in thread to {authorName}"**.
  - Editing: "Edit message".
  - Session: "Message this session".
- **Formatting inside the input:**
  - Inline code: same as the timeline.
  - Code block: radius 4px, fill `--affordance-subtle`, padding 8px 12px.
  - Blockquote: **3px** `--text-subtle` left border with 12px padding.
  - Spoiler: `--affordance-subtle` fill with a dotted underline.
  - Emoji-only draft: 42px glyphs on a 48px line.
- **Mention chips inside the input** (`.inline-chip`, `src/shared/design-system/styles/chips.css`):
  - Fill `--affordance-subtle`, text `--text-standard`, radius 4px, padding-inline 3px, line-height 1.35, max-width 256px.
  - Label "@Name".
  - Hover fill `--affordance-subtle-hover`.
  - Unresolved chips use fill `--surface-inset` and text `--text-metadata`.

**Edit mode header**

- Contents: `pencil` (18px), then "Editing message", then an X `sm` button ("Cancel edit").
- Box: margin -8px -12px 4px, padding 8px 12px, fill `--surface-inset`, label-sm 14px/500 in `--text-secondary`.
- The shell gets `overflow:hidden`.

**Attachments preview row** (above the input, `ComposerAttachments.module.css`)

- **Row:** horizontal scroll, gap 8px.
- **File item:** 288px wide × 46px tall, padding 2px, border 1px `--border-standard`, radius 12px, gap 8px.
- **File icon box:** 40×40, fill `--affordance-subtle`, radius 9px, `file-text` icon at 24px.
- **Text:** name in label-sm 14px/500 with ellipsis; below it the hint "{size} · {status}" as a 12px caption in `--text-subtle`.
  - Size: "12 KB" or "1.2 MB".
  - Status: "Queued", "Preparing…", "Uploading…", "Ready" or "Upload failed".
- **Image or video item:** a 46×46 thumbnail tile with radius 12px and a 0.5px inner border.
- **Item actions** (top-right; on media tiles inset 2px on a pill backdrop):
  - Remove: `xs` 20px prominent (black/white) button with a 12px X. While uploading it shows a `loader-2` spinner (700ms per turn) instead.
  - Optional `alert-circle` issue button and `refresh` retry button.

**Actions row** (`.composerActions`)

- Flex wrap, `align-items:center`, gap 8px, padding-top 4px.
- The tools area is `flex:1`, min-height 36px, gap 4px, which pushes Send to the right edge.

Toolbar buttons, left to right (32×32, 16px glyph unless noted):

| # | Button | Tabler icon | Notes |
|---|---|---|---|
| 1 | "Mention a member" | `at` | Leading group (`.composerLeadingTools`: gap 2px). When explicit recipients exist, the group becomes a pill (fill `--affordance-subtle`, padding-inline 2px) and also shows recipient buttons: 28×28 with a 2px `--affordance-subtle` border, a 24px avatar inside, overlapping by -6px. Hovering one shows an X overlay on `--affordance-prominent`. |
| 2 | "Attach files" | `paperclip` | Opens the file picker |
| 3 | "Emoji and GIFS" | `mood-smile` | Emoji or GIF picker |
| 4 | "Issues and pull requests" | `git-pull-request` | Only when the channel is linked to a project |
| 5 | "Record voice note" | `microphone`, **18px** | Plain 32px button, glyph `--text-subtle`, radius 10px, no hover fill, press scales to 0.96 |
| 6 | "Toggle formatting" | `letter-case` | Ghost; `aria-pressed` while open |
| — | (spacer) | | |
| 7 | "Send message" / "Save changes" | `arrow-up` | See below |

Send button states:

| State | Appearance |
|---|---|
| Empty draft | Ghost and disabled: transparent, glyph `--text-unavailable` |
| Has text or attachments | **Prominent**: fill `--affordance-prominent` (#000 light, #fff dark), glyph `--text-inverse`. Hover `--affordance-prominent-hover`; pressed #525252 (light) / #b8b8b8 (dark). |
| Busy (sending or admitting) | Disabled |

Shape is 32×32 with radius 10px. Enter sends; Shift+Enter inserts a newline.

**Formatting mode**

- Clicking "Toggle formatting" swaps the tools for:
  - "Close formatting": X, subtle variant (`--affordance-subtle` fill).
  - A divider: 1×20px `--border-standard` at opacity .6, margin-inline 4px.
  - A horizontally scrolling set of format buttons with a 2px gap.
- Format buttons, in order:
  1. Bold (`bold`)
  2. Italic (`italic`)
  3. Strikethrough (`strikethrough`)
  4. Code (`code`)
  5. Code block (`source-code`)
  6. Link (`link`)
  7. Bullet list (`list`)
  8. Ordered list (`list-numbers`)
  9. Quote (`blockquote`)
  10. Spoiler (`spy`)
- Active formats use the subtle fill.
- Transition: 140ms `cubic-bezier(.23,1,.32,1)`. The tool group slides in from x = -12px; the format group fades and scales from .95.

**Reply context (thread)**

- Box: flex row, gap 8px, padding 8px 12px, fill `--surface-inset`, radius 12px, 12px caption.
- Text: "Replying to {name}", then the excerpt on one line with ellipsis in `--text-subtle`.
- Trailing: an X `sm` button ("Cancel reply target").

---

### 4.9 Autocomplete popups

Sources: `src/features/conversation/ComposerCompletions.tsx:170-460`, `Completions.module.css`, `useCompletionPosition.ts`, `src/bundled/mentions/*`, `src/bundled/channels/ChannelCompletion.tsx`, `src/bundled/emoji/EmojiCompletion.tsx` and `emoji-query.ts`.

| | @ mention | # channel | :emoji: |
|---|---|---|---|
| Trigger | `@` + query | `#` + query | `:` + 2–64 characters of `[a-zA-Z0-9_+-]`, after the start of the line, whitespace or `([{` |
| Width | Composer form width, max 380px | Form width, max 480px | 0.375 × form width |
| Gap to composer | 4px; above by default if at least min(menu height, 160px) fits | 4px; above by default | 8px; on whichever side has more room |
| Popup padding | 12px | 12px | 6px |
| Row | padding 8px, gap 8px, 32px avatar, **48px tall**. List is a grid with 4px gap. | Same as mention; 22px hash or lock icon in a 32px box | padding 6px 8px, 28px preview (native emoji at 24px), **40px tall**. Label `:shortcode:` on one line; detail hidden. |
| Item cap | 50 | 20 | Unlimited (scrolls) |
| List max-height | 280px | 320px | 280px |

- **Surface:** `popover-surface`, i.e. `position:fixed`, border 1px `--border-standard`, radius 16px, elevated glass fill, `--shadow-sm`, z-index 1100.
- **Horizontal position:** left edge aligned with the form's left edge, clamped 8px inside the visual viewport. The max-height is the space available on the chosen side.
- **Highlighted row:** fill `--affordance-popover-selected` (light #f5f5f6, dark #404040). Row radius is the inner radius: 16 − inset − 1, i.e. 9px for emoji and 3px for mention and channel.
- **Pointer:** hovering moves the highlight.
- **Mention row detail:** under the name, a `<small>` 12px `--text-subtle` line. For channel members it shows the pubkey hex in mono `--text-metadata`, clipped to 12ch.
- **Channel row:** the highlighted row shows an "Enter" key chip at the far right: padding 4px 8px, radius 4px, fill `--surface-inset`, 12px `--text-subtle`.
- **Status:** status or retry text is a 12px caption in `--text-subtle` with margin 4px 8px. A retry row reads "Retry suggestions".
- **Keys:** ↑/↓ moves (wrapping), Enter or Tab accepts, Esc dismisses. Space accepts an exact mention or channel match; ":" accepts an exact emoji.
- **Slash commands:** none are registered.

---

### 4.10 Typing indicator

Sources: `TypingIndicator.tsx:166-222` and `TypingIndicator.module.css`.

- **Position:** absolutely positioned in `.composerContext`, which sits directly above the composer. Its bottom edge is **8px above the composer's top edge**, inset 16px left and right (12px at ≤650px). It overlays the bottom of the timeline with `pointer-events:none`.
- **Layout:** flex `row-reverse`, `space-between`, gap 6px. Typing humans sit on the left; the working-agents chooser sits on the right.
- **Humans pill:**
  - Fill `--surface-panel` (no border or shadow), radius 12px, padding 4px 8px, gap 8px, max-width 50%.
  - Text: 12px caption in `--text-subtle`, single line with ellipsis.
  - Dots: three 4×4 circles in `currentColor`, 2px gap, pulsing to opacity .35 at 50% (1.4s ease-in-out, delays 0 / 0.2s / 0.4s).
- **Text format:** a plain "typing" suffix with no ellipsis character.
  - "Alice is typing"
  - "Alice, Bob are typing"
  - "Alice, Bob and N others are typing" (first 2 names, then the remaining count)
- **Agents chooser:**
  - Ghost `xs` button, min-height 32px, padding 4px.
  - Up to 4 agent avatars at 24px, each with 2px padding, `--surface-panel` fill, radius 12px and `--shadow-sm`, overlapping by -4px. "+N" overflow.
  - Hover (250ms) opens a "Working now" popover.
  - The screen-reader text reads "{names} is working" / "are working".

---

### 4.11 Thread panel

Sources: `ChannelsPage.tsx:1556-1674`, `src/features/panels/PanelWorkspace.tsx`, `Panels.module.css`, `usePanelSplit.tsx`, `ResizeHandle.module.css`, `ThreadPanel.tsx`, `ReplyBranch.tsx`, and `Messages.module.css:676-750, 1683-1796`.

**Placement:** opens in the **right dock** (`.panelStack`, grid column 2, left border 1px `--border-standard`) as a tab of the dock's panel workspace.

| Width setting | Value |
|---|---|
| Default | The board splits `minmax(320px,1.1fr) : minmax(316px,1fr)`, so the dock is about 47.6% of the width |
| After resize | `--secondary-panel-width`, clamped to [316px, available − 320px]. Double-click resets. |
| Resize handle | 16px hit area centred on the border. On hover, a 3×56px `--border-focus` pill fades in. |
| Viewport ≤1000px or container ≤651px | Absolute overlay on the right: width min(420px, 100%), `--shadow-sm`, slides in 100% over 180ms `--easing-enter` and out over 120ms |
| ≤650px | Full width, no border |
| Open / close motion (desktop) | `translateX(12px)` + opacity over 180ms `--easing-enter`. Reduced motion: opacity only. |

**Header** (PanelWorkspace)

- A `panel-header` (40px, hairline at the bottom) holding a navigation tab strip.
- Each tab is 192px wide, radius 10px, padding 4px 8px, 14px text.
- The selected tab uses fill `--affordance-subtle`.
- The thread tab is labelled **"Thread"** with a `message-circle` icon (16px).
- **Close:** an `xs` X button (20px, 16px glyph), 4px from the tab's right edge and vertically centred. On unselected tabs it appears only on hover or focus. Esc also closes.
- An "Add tab" `plus` button (32px) fades in when the header is hovered.
- When the thread panel is used outside the tab host, it renders its own header: title "Thread" (16px/500 label) and a "Close thread" X button (32px, autofocused).

**Body** (`section.threadHistory`)

- Scrolls with padding 0 24px, bottom 12px.
- **Root message:** `layout="thread"`, always preceded by a day divider.
  - Avatar 32×32 (size `default`; when clickable, a 32px round button), gap 8px.
  - Same padding as the timeline (14px / 2px).
  - Inline images or video use thread mode.
- **Replies:** a tree of `li.replyItem`.
  - Top-level replies: 32px avatar.
  - Nested replies (depth > 0): 24px avatar centred in a 32px box.
  - Continuations follow the same 5-minute rule; in threads the gutter is 32px with gap 8px.
  - Day dividers appear between replies on different days.
- **Branch indentation:**
  - Child list indented 40px.
  - Connector lines: 1px `--border-standard`, with an L-shaped elbow whose bottom-left corner has radius 12px. The spine runs at x = 16px under the parent avatar centre.
  - Indentation flattens at depth 6, and at depth 3 or 4 in narrow (≤360px or ≤480px) panels.
- **Collapsed branch summary:**
  - A link-variant `sm` button indented 40px.
  - Contents: up to 3 avatars, then "N replies", then "(N new)" when there are unread replies.
- **Status text** (12px `--text-subtle` thread notes):
  - "Loading thread…"
  - "Original message unavailable."
  - "Thread history limit reached."
  - "Earlier reply unavailable in loaded history."
  - "Retry thread" button.
- **Jump to latest:** the same button as the timeline.

**Composer:** the same composer as the channel (margin 0 16px 16px), pinned at the bottom.

- Placeholder: "Reply in thread to {root author}".
- Choosing Reply on a specific reply adds the "Replying to …" context bar.
- There is **no "Also send to channel" checkbox**. Instead, your own thread replies offer "Send to channel" in the ⋯ menu. The copy posted to the channel shows "Sent from Thread — {excerpt}" above its body.

---

### 4.12 Read-only and join notice

Source: `src/bundled/channels/ChannelJoinNotice.tsx`.

- Rendered between the timeline and the composer for read-only (preview) channels.
- Box: flex wrap, gap 12px, padding 8px 16px, 14px body-sm in `--text-subtle`.
- Text: "Read-only preview · You haven’t joined this conversation."
- For joinable stream or forum channels, adds a "Join channel" `sm` button (subtle, 32px). An error appears inline after the button.

## 5a. Agents page

All paths are relative to `/home/ben/projects/buzz-app`. 1rem = 16px at the default
text scale. Root `font-size` is `calc(100% * var(--buzz-text-scale, 1))`
(`src/shared/styles/globals.css:409-413`), so every rem value below scales with the
user's font-scale setting.

### 5a.1 Registration and render paths

Source: `src/bundled/agents/index.tsx`, `src/bundled/agents/AgentsPage.tsx`.

- Page id `agents`, title `Agents`, `layout: "workspace"`, `companion: true`,
  `primary: true` (`index.tsx:16-25`). The shell nav icon is `RobotIcon`
  (the custom Soft & wide mark, `icons/Robot.tsx`), registered in `src/app/shell/presentation.ts`.
- `AgentsPage` picks one of four bodies (`AgentsPage.tsx:300-480`):

| Condition | Body |
|---|---|
| No `control` (web, no desktop host) | `<p class="text-secondary">` "Open the desktop app to import and run agents. You can still mention existing channel members." + teams + old library |
| `control` present, `state.status === "unavailable"` | teams + old library (`AgentLibrary`) |
| `state.data.parked !== undefined` (the normal desktop path) | `UnifiedInventory` -> `InventoryView` (sections of `AgentCard`s) |
| Otherwise (older host) | `ManagedAgents` grid (`AgentsPage.tsx:483-534`) |

- When the relay is not ready, the old library slot shows "Connect to a community to browse the old library.". If `connection.status === "error"`, it also shows a `Retry connection` button (`AgentsPage.tsx:247-256`).

### 5a.2 Page frame and header

Sources: `AgentsPage.tsx:276-299`, `src/shared/design-system/ui/PanelHeader.tsx`,
`src/shared/design-system/styles/components.css:744-800`, `src/shared/design-system/styles/tokens.css:726-746`.

| Element | Spec |
|---|---|
| Surface | `FullPageSurface aria-label="Agents"`, wrapped in `PanelFrame` (companion = profile `PanelCard` when "View profile" is used) |
| Header | `.panel-header`: `min-height: 40px` (`--size-panel-header: 2.5rem`), flex, `justify-content: space-between`, gap 8px, `padding-inline: 4px`. 1px bottom rule (`::after`, `--border-standard`), inset 4px left and right |
| Title | `<h2 class="text-label text-primary">Agents</h2>`: 16px / 24px line height, weight 500, letter-spacing 0, ellipsis |
| Header actions slot | `<div class="flex items-center gap-2">` (gap 8px). Other components portal into it (see 5a.3) |
| Scroll body | `flex-1 overflow-auto p-panel-inset text-body`: padding 24px all sides; body text is 14px / 20px |
| Content column | `mx-auto flex w-full max-w-6xl flex-col gap-section-gap`: max-width 72rem (1152px), centered, vertical gap 32px |
| Controls section | `<section aria-label="Local agent controls" class="agent-controls flex min-w-0 flex-col gap-section-gap text-body text-primary">` (gap 32px) (`AgentControlPanel.tsx:362-366`) |

### 5a.3 Header buttons and menus

| Control | Source | Spec |
|---|---|---|
| **Add agent** | `AgentControlPanel.tsx:319-331` | `Button variant="subtle" size="sm"`: min-height 32px, padding 6px 16px, capsule radius 26px, `text-label-sm` (14px / 500), fill `--interaction-fill`. Content: Tabler `plus` 16px + " Add agent". Opens `AgentCreateDialog`. Portaled first, so it sits left of the overflow menu |
| **Page overflow menu** | `UnifiedInventory.tsx:350-372` | `IconButton size="sm"` (ghost, 32x32, radius 10px). Icon: Tabler `dots` (`DotsThreeIcon`, size prop 18, drawn at 16px by button CSS). `aria-label="Agent page actions"`. `MenuPopup align="end"` holds one item, `Refresh agents` (disabled while the library is loading) |
| Fallback when no header slot | `UnifiedInventory.tsx:374-386` | Inline `Button` "Refresh agents", `self-start` |
| Legacy path (`ManagedAgents` / library) | `AgentLibrary.tsx:62-74` | `Button variant="quiet" size="compact"`: Tabler `refresh` 16px + "Refresh agents" (reads "Retry" on error) |

Button metrics come from `src/shared/design-system/styles/components.css:9-210`.
Inside `.buzz-button-label`, CSS forces child SVGs to 24px (md), 16px (sm/compact/xs)
or 12px (icon-size xs), whatever the `size` prop says.

### 5a.4 Inventory layout (normal desktop path)

Source: `src/bundled/agents/InventoryView.tsx:189-319`, `src/bundled/agents/inventory-decisions.ts:4-13`, `src/bundled/agents/AgentControls.css`.

The root is `<section aria-label="My agents" class="flex flex-col gap-6">` (gap 24px). It renders, in order:

1. Status and alert lines (children from `UnifiedInventory`, see 5a.9).
2. Empty states (see 5a.10).
3. The **"Local agents in this community"** group, with heading text **"Individual agents"**, rendered as a tile grid.
4. Teams contribution (from the channel-templates plugin, if any).
5. The other groups, in this order: "Local agents in other communities", "Available to import", "Relay-only agents". These render as **row lists**.
6. "Archived (N)": an Accordion (`headingLevel 2`). It opens by default only when every agent is archived. Its groups are nested one heading level deeper.
7. "Profiles without identities": `<h2 class="text-heading">` (24px / 500) with a tile grid.

Group markup (`InventoryView.tsx:169-243`):

| Part | Classes | Values |
|---|---|---|
| Group section | `flex flex-col gap-3` | gap 12px |
| Group heading | `h2` (`h3` nested), `m-0 text-label-sm` | 14px / 20px, 500 |
| Community subsection | `flex min-w-0 flex-col gap-2` | gap 8px. Used only in the "other communities" and "relay-only" groups. Sorted; empty community reads "Community unknown" |
| Community heading | `h3`/`h4`, `m-0 break-all text-label text-secondary` | 16px / 24px, 500, `--text-subtle` |
| Tile container (local-here group) | `.agent-grid` | see below |
| Row container (other groups) | `overflow-hidden rounded-xl border border-primary` (+ `agent-relay-inventory` for relay group) | radius 12px, 1px `--border-standard` |

Sorting: case-insensitive by display name, then by pubkey (`InventoryView.tsx:68-86`).

#### Tile grid

`AgentControls.css:1-5`:

```css
.agent-grid { display:grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 10.75rem), 1fr)); gap: 16px; }
```

The minimum tile width is 172px and the gap is 16px. N columns fit when the content width is at least `188N - 16`px:

| Content width (inside 24px padding) | Columns |
|---|---|
| < 360 | 1 |
| 360-547 | 2 |
| 548-735 | 3 |
| 736-923 | 4 |
| 924-1111 | 5 |
| 1112-1152 (max) | 6 |

Tiles stretch (`1fr`) to fill the row.

#### Row list

From `AgentControls.css:16-91`:

- `.agent-inventory-row`: grid `minmax(0,1fr) minmax(0,auto)`, `align-items:center`, gap 16px, padding 12px 16px.
- Adjacent rows are separated by a 1px top border (`--border-primary` = `--border-standard`).
- The actions column is right-aligned.
- At <=640px the row is one column with gap 8px and actions left-aligned. The relay group keeps two columns.
- Details toggle (`.agent-inventory-details > summary`): 32x32, radius 12px (`--radius-control`), hover `--bg-hover`, 2px `--border-focus` focus outline. Chevron-down 18px rotates 180deg when open.

### 5a.5 Agent card (tile) anatomy

Sources: `src/bundled/agents/AgentCard.tsx`, `src/bundled/agents/AgentCard.module.css`, `src/features/agents/AgentAvatar.tsx`, `src/features/agents/agent-avatar.css`.

| Part | Spec |
|---|---|
| Card `.card` | flex column; 1px `--border-standard` (light `neutral-3`, dark `neutral-4`); radius 16px (`--corner-container`); `min-height: 15.25rem` (244px) |
| Content `.content` | flex column, centered on both axes, `text-align:center`, gap 12px, padding 32px 12px, radius 16px. Hover/focus of the full-card overlay button paints `--surface-hover` (`--affordance-subtle-hover`) |
| Overlay button `.open` | `position:absolute; inset:0`, transparent. Label "View profile for {name}" (opens the profile companion) or "Manage {name}" (opens the Manage dialog). Present only if `onViewProfile` is set or the card has controls |
| Portrait `.portrait` | 80x80 (5rem). `AgentAvatar shape="squircle" size="fill"`. Squircle is an SVG mask (`src/shared/design-system/styles/avatar-squircle.svg`). Fallback is the uppercase first character of the name on `--affordance-subtle`, color `--text-subtle` |
| Name | `<h3/h4/h5 class="m-0 truncate text-label-sm">`: 14px / 20px, 500, single-line ellipsis, `title` = name |
| Restart badge (when `restartDiff` non-empty) | `mt-1` wrapper (4px). Pill: `inline-flex gap-1 rounded-full border border-warning-border bg-warning-surface px-2 text-caption text-warning`. Icon Tabler `refresh` 14px, text "Restart required", `role=status` |
| Archived pill | `inline-block rounded-full border border-primary px-2 text-caption text-secondary`: "Archived" |
| Model line (tile, managed agent only) | `<p class="m-0 text-caption text-subtle model">`: 12px / 16px, `--text-subtle`, `margin-top:4px`, single-line ellipsis. Text is the first that applies: `launchModel`; "Custom model" (model set via env); `harness.model`; "Default model" |
| Actions menu button | Absolute `top:8px; right:8px; z-index:10`. `IconButton size="compact"` (32x32 ghost, radius 10px), Tabler `dots`, aria "Actions for {name}" |
| "Review agent status" button | Shown under the content in `px-4 pb-4` (16px sides, 16px bottom) when the agent was just imported, has an error, has a pending profile, or has a notice. `Button size="compact"` (32px) |
| Feedback slot | Full width, `px-4 pb-4`. Shows "Archiving…" / "Unarchiving…" (`text-body-sm text-secondary`), or "{Archive\|Unarchive} failed: {error}" (`text-danger`) with a compact `Retry` button |
| No identity | `<p class="m-0 mb-6 text-center text-caption text-subtle">No linked identity</p>` (24px bottom margin) |

The harness name is **not** shown on the tile. Only the model line is.

Row layout (`layout="row"`) uses the same component with `.agent-inventory-row`:

- Left: `flex items-center gap-3` (gap 12px) with a 32x32 squircle avatar (`AgentAvatar size="default"` -> `.agent-motion-avatar` 2rem) and the name block. Adds `pr-6` (24px) when a menu exists.
- Right: `.agent-inventory-actions flex flex-wrap items-center gap-2` (gap 8px) holds the inline actions (`pr-8` = 32px when a menu exists).

#### Card actions menu

`AgentCard.tsx:226-386`. `MenuPopup align="end" size="wide"` (max-width `min(28rem, available)`).
Menu icons are 14px, inside `MenuIcon`. Items in order:

| Item | Icon | Condition |
|---|---|---|
| View profile | none | same-community agent with a profile panel |
| Manage agent | none | tile with controls |
| Archive agent / Unarchive agent | `archive` / `archive-off` | archive is available in the connected community. Disabled while pending |
| *separator* | | if one of the above and Edit |
| Edit | `pencil` | one editable setup. With several setups, the label is a `ChoiceRow` "Edit {name}" with the relay URL and npub (mono) as the description |
| Duplicate | `copy` | |
| Share | none | |
| *separator* | | |
| Delete | `trash` | `tone="danger"`, only if host supports delete |
| Edit (disabled) + note | `pencil` | when nothing is editable. Note reads "Import this identity to edit in Foundation." or "No linked identity to edit." |

Cards with identities and no actions show a popover instead: title "{name} identity details" and a list of npubs (`text-mono-sm text-subtle`, select-all) (`AgentCard.tsx:448-474`).

#### Manage dialog (tile controls)

`AgentCard.tsx:404-417`: `Dialog title="Manage {name}"`, body `flex flex-col gap-3` (12px). The body comes from `InventoryIdentityCard.tsx:243-387`, one block per local setup (`ManagedAgentActions.tsx:37-140`):

1. Relay URL (`text-body-sm text-secondary`, break-all). Omitted when the section already names the community.
2. Process label (`text-body-sm`), prefixed with "Last known: " when the control state is in error. See 5a.6.
3. Optional lines: "Imported, not started. Start it when you are ready." (or "…Choose Use here to set up this identity in a community."), "Starts with this app.", the error text, "Checking agent status…", the notice, and "Settings saved. Profile publication is pending." with a `Retry profile` button.
4. Button row `flex flex-wrap gap-2`:
   - Stopped or failed: **Start** (`primary`/prominent, compact). Reads "Retry start" when failed.
   - **Stop** (subtle, compact). Disabled unless stoppable.
5. Launch-block reason (`text-body-sm text-secondary`), e.g. "Refresh status before starting.", "Waiting for the current operation.", "The bundled agent runtime is unavailable.", "Waiting to start; unlock Keychain if prompted.", "Waiting for the process transition.", "Choose Use here before starting this imported identity." (`src/features/agents/control.ts:380-397`).

Then the identity actions (`flex flex-wrap items-center gap-2` in a tile):

- an optional installation `<select>` ("Choose an installation" / "Installed Buzz" / "Development Buzz");
- **Import** (primary compact);
- **Use here** (compact);
- **Clone** (subtle compact, tooltip "Create a new agent from this agent’s name and instructions, with a new identity and key. Memories and history are not copied.");
- **Remove** (destructive compact, relay-only rows).

Last comes a `<details>` with the summary "Identity & sources". In row layout the summary is a chevron button instead. It lists the communities, "Installed Buzz · Development Buzz", and the npub (`text-mono-sm`, select-all).

### 5a.6 Status indicators

There are three separate systems:

**(a) Avatar presence dot.** Sources: `src/shared/design-system/ui/Avatar.tsx`, `components.css:448-478`, `avatar-status.css`, `tokens.css:539-544`, `612-617`.

| State | Dot fill | Notes |
|---|---|---|
| `online` | `--status-avatar-online` (green-10), with a 1px `--status-avatar-online-border` (green-11) ring via `::after inset:1px` | On an agent squircle avatar, `online` is replaced by the thinking-badge "available" notch (b) |
| `away` | `--status-away` (amber-10) | |
| `offline` | `--status-offline` (light neutral-8, dark neutral-10) | |
| `unknown` | no dot | `AgentCard` passes `undefined` |

Geometry: centered at `top:85%; left:85%`, size 25% of the avatar, `translate(-50%,-50%)`. The squircle avatar dot is squircle-masked. The avatar is masked with `avatar-status-squircle-cutout.svg` to cut the gap.

**(b) Thinking badge.** Shown on squircle agent avatars when online or working. Source: `src/shared/design-system/ui/agent-thinking/ThinkingBadge.tsx`, `thinking.css`, `badge-cutout-geometry.ts`.

- It is drawn in an 88x88 design box scaled to the avatar size. On an 80px card that is `scale(80/88)`.
- **Available:** a 22x22 rounded square (radius 7px) centered at (74.8, 74.8), filled `--status-online` (green-11). A clip-path notch with a gap of `min(6.6, 3*88/size)` is cut out of the avatar around it.
- **Thinking:** the ink morphs into a pill centered at (44, 88), the bottom center. The pill is the size of a `sm` prominent "Start" button (measured offscreen), with radius = height/2 and fill `--affordance-prominent` (neutral-12).
  - The morph takes 300ms. Position uses ease-out-quint `1-(1-t)^5`; dot opacity uses ease-out-cubic over the first third.
  - The fill-color transition is 300ms `cubic-bezier(0.22,1,0.36,1)`.
- **Dots:** three 7x7 circles, gap 4px, color `--text-inverse`. Animation `badge-pill-typing-pulse` is 1.2s linear infinite, with delays 0/80/160ms:
  - at 0%, 40% and 100%: opacity .4, `translateY(0)`;
  - at 20%: opacity 1, `translateY(-2px)`.
- **Reduced motion:** the transition is instant, and the dots use a fade only (opacity .55 to .85).
- **Accessible name:** "{name}, thinking" or "{name}, available".

**(c) Process status text.** No color: `text-body-sm`, inherited color. Source: `src/bundled/agents/agent-edit.ts:113-128`.

| `AgentView.status` | Label |
|---|---|
| running | "Process running · relay readiness unverified" |
| waiting | "Waiting to start · unlock Keychain if prompted" |
| starting | "Starting process" |
| stopping | "Stopping process" |
| failed | "Process failed" |
| stopped | "Process stopped" |

Other status-colored text in agent UI:

- `text-warning` (amber-12) for model warnings.
- `text-danger` for errors.
- `text-success` + Tabler `circle-check` 16px for "Connected. The model replied.". Test failures show `alert-circle` 16px (`AgentModelPicker.tsx:332-353`).
- Spinners use Tabler `loader-2` with `motion-safe:animate-spin`. The button spinner spins at 1s linear (`components.css:50-53`).

### 5a.7 Add agent dialog (create / duplicate / clone)

Source: `src/bundled/agents/AgentCreateDialog.tsx:385-589`, fields in `AgentSettingsFields.tsx`, `AgentHarnessEditor.tsx`, `AgentModelPicker.tsx`, `ProviderApiKeyField.tsx`, `AgentEnvironmentEditor.tsx`. Dialog chrome comes from `src/shared/design-system/styles/overlays.css:2-221`.

**Chrome**

- `.buzz-dialog.agent-dialog`: width `min(53.75rem, 100vw - 2rem)` (860px), max-height `100dvh - 48px`, centered.
- Padding 24px, flex column, gap 16px, radius 20px (`--corner-panel`), 1px `--border-standard`, `--shadow-sm`, elevated material. Backdrop is `--bg-scrim`.
- Motion: opacity plus `scale(0.98)` on enter and exit (`--duration-state` / `--duration-settle`).
- Header: `.buzz-dialog-header` (flex, space-between, gap 16px). Title is `text-heading` (24px / 1.1 / 500 / -0.0075em):
  - "Add agent";
  - "Duplicate {source.name}";
  - "Clone agent" (seeded from clone settings).
- Description (`.buzz-dialog-description`, 14px, `--text-subtle`): "Create and start an agent in {destination \|\| "a connected community"}. It won't join a channel automatically."
- Clone notice (`text-body-sm text-secondary`): "Only the name and instructions were copied. Review them for embedded secrets. Choose this computer’s workspace and runtime settings. Identity keys, environment values, history and community membership are not copied. The source stays unchanged."

**Two-pane layout** (when import or catalog is available). From `AgentControls.css:93-123`:

- `.agent-add-layout`: grid `12rem minmax(0,1fr)` (192px sidebar).
- `.agent-add-sidebar`: flex column, gap 4px, max-height `min(70vh, 42rem)`, scrolls, 1px right border, padding 12px.
- At <=640px it stacks: sidebar max-height 160px, bottom border.

Sidebar items, in order:

1. `NavigationItem` "Create agent" (aria "Create new agent"), Tabler `plus` 16px. Selected by default. Row min-height 32px, radius 8px.
2. `Button variant="outline" size="sm"` "Import". Opens "Import agent snapshot".
3. Label "AGENTS" (`text-label text-subtle`, padding 12px). Shown only if there are catalog entries.
4. One `NavigationItem` per catalog agent, with a 24px circle `Avatar` icon.

Selecting a catalog agent replaces the form with a preview. The button there reads "Add agent", or "Added to My Agents" when already added.

**Form fields, in order.** The form is `.buzz-dialog-body space-y-section-gap` (32px between blocks). Inside a block, spacing is `space-y-4` (16px). `Field` is a grid with gap 8px. Labels are `text-label-sm` (14px / 500), descriptions `text-body-sm` `--text-subtle`, errors `text-body-sm` `--text-danger` (`forms.css:28-47`).

| # | Label | Control | Default | Placeholder / helper / notes |
|---|---|---|---|---|
| 1 | Name | `Input` | "" (Duplicate: "{name} copy"; seed: seed name) | none |
| 2 | Agent instructions | `Textarea rows=6` | "" or seed `systemPrompt` | none |
| — | **AI configuration** | `<fieldset>` legend `text-label` (16px/500), `mb-4` | | |
| 3 | Harness | `Select variant="field"` | Agent-defaults harness if available, else `buzz-agent` | Options are the host `harnessOptions` labels, e.g. "Buzz Agent", "Goose", "Pi", "Hermes Agent", "Claude Code". Unavailable options get the suffix " (install first)" and are disabled. Last option: "Custom executable / current value", which reveals `Field "Executable"` (Input). Helpers: "Pi needs its CLI, Node.js and buzz-pi-acp before you can select it."; "{preset} needs its ACP launcher. Set it up under Settings → Agents → Harnesses."; link button "Open Harnesses in Settings" with the note "Opening Settings discards unsaved edits." |
| 4 | Provider (label is "LLM Provider" for Goose, Pi and external harnesses) | `Select variant="field"` | `""` when the harness is not buzz-agent or defaults supply a provider; else the first provider (`databricks_v2`, label "Databricks v2" in fixtures) | First option is "Use agent defaults ({provider})" or "Not set". Then the harness providers. Pi lists signed-in providers, then "{label} (API key needed)". Last option: "Custom provider / current value", which reveals `Field "Custom provider"`. "Loading signed-in providers…" shows while Pi loads. Hidden for preset harnesses (Hermes/Claude) |
| 5 | {label} API key (e.g. "OpenAI API key") | `InputGroup` holding a password `Input` (`autoComplete=new-password`). Trailing `IconButton size="sm"` with Tabler `eye` / `eye-off` 16px ("Show API key" / "Hide API key"), shown only once text is typed | "" | Placeholder: "Paste API key" (Buzz Agent), "Paste API key or use an existing Pi sign-in", or "Paste API key or use existing Goose credentials". Becomes "Saved key unchanged" when a key is saved, or "Will remove on save". Helper: "`{ENV}` is required for OpenAI. Leave blank to keep a saved key, if present, or use one from Agent defaults. Keys exported in your shell profile are not used. Saved keys are stored in this device’s local agent settings files." Only shown for providers needing a key |
| 6a | (preset harness) | text | | "{label} uses its own default model and sign-in. {setupHint}". If a model or provider is set: alert "This agent has model or provider settings that Buzz cannot apply to {label} yet…", then "Current model: `x`" and a "Use {label} defaults" button |
| 6b | Model | `Combobox`: input plus trigger "Browse models" (chevron), spinner while loading | "" | Placeholder "Use agent defaults ({model})" or "Choose or enter a model". Popup list max-height `min(20rem, available-4rem)`. Item = name + description `{id}` (" · Custom ID" if custom). Empty: "Type a model ID to use a custom model.". Loading row: `loader-2` 16px + "Loading models…" / "Loading Pi models…". Optional "Test connection" button above (external harness with a provider). Status line plus "Cancel model lookup" / "Cancel sign-in" / "Retry models" |
| 6c | **Advanced** | `h3 text-label`, margin-top 32px, margin-bottom 8px; then `Accordion variant="form"` (`-mx-2`), item "Model" | | |
| 6c-i | Model ID (custom or blank) | Input | `draft.model` | Pi helper: "This field uses the exact model ID…". Button "Refresh models" (Pi) |
| 6c-ii | Databricks workspace (HTTPS origin) | Input | "" | Placeholder "https://workspace.example.com", or "Use agent defaults" when inherited (Buzz Agent only) |
| 6c-iii | Model filter (optional) | Input | "" | Placeholder "Use agent defaults" when inherited. Helper "Editing either field saves both displayed values."; buttons "Refresh models" and "Disconnect"; note "Credentials are shared within Foundation for this workspace, not with old Buzz. Disconnect removes this app’s cache, not your browser session." |
| 7 | Conversation context | `Select variant="field"` | "" (inherit) | Options: "Use agent defaults (Each thread)" (or "(Entire channel)"), "Entire channel", "Each thread". Description: "Entire channel shares one conversation across threads. Each thread keeps a separate conversation; direct messages remain shared." |
| — | (owner-only build note) | text | | "This build allows instructions only from the owner and verified same-owner agents." |
| — | **Environment** | `Accordion variant="form" keepMounted` (`-mx-2`), collapsed | | |
| 8 | Workspace | Input, `spellCheck=false` | `state.data.defaultWorkspace` | none |
| 9 | Arguments (JSON array) | `Textarea rows=3` | `JSON.stringify(harness.defaultArgs ?? [])`, e.g. `[]` | none |
| 10 | Environment overrides | `<fieldset>` legend `text-label` | `{ BUZZ_ACP_AGENTS: "10" }` (create only), so one row is pre-listed | Helper: "Saved values stay on the host. Leave unchanged to preserve them; replacements are write-only. Remove takes effect only when you save. Do not put secrets in arguments or prompts." Each key row is `flex flex-wrap items-end gap-2`: a mono label (`text-mono`), a password input (text for `BUZZ_ACP_AGENTS`), and `Remove` / `Undo` buttons. Placeholders: "New value", "Saved value unchanged", "Will be removed on save". Row state: "Will replace on save" / "Will save an empty value" |
| 11 | Variable name | Input + `Add variable` button | "" | Errors: "Use letters, numbers and underscores; start with a letter or underscore." and "That variable is already listed." |
| — | footer helper | text | | "Environment overrides take precedence over provider and model selections. Arguments are passed literally, not through a shell." (Pi and preset variants also exist) |

Submit validation is thrown by `agentEdit` (`agent-edit.ts:71-112`) and shown as `<p role="alert">` above the footer:

- "Enter an agent name."
- "Enter a harness executable."
- "Enter a workspace path."
- "Use {preset} defaults before saving this agent."
- "Arguments must be a JSON array of strings."
- "ACP arguments must be nonempty and cannot contain commas."

Status lines (`role=status`):

- "Creating agent…"
- "{name} was created. Starting it…"
- "{name} was saved. Finishing its profile…"
- "Checking agent status…"
- "{name} was saved. Start it to finish setup."
- "Connect to a community and use a rebuilt desktop app to create an agent."
- Runtime alert: "This app’s agent runtime is unavailable. Repair or rebuild the desktop app before creating an agent."

**Footer.** `.buzz-dialog-actions`: flex, wrap, `justify-content: safe end`, gap 12px.

| Button | Variant / size | Label |
|---|---|---|
| Secondary | subtle, md (40px min-height, padding 8px 24px, capsule) | "Cancel". Reads "Close" when busy or saved |
| Primary (submit) | prominent (`--affordance-prominent` fill, `--text-inverse` text), md | "Create agent" or "Clone agent". During work: "Creating…", "Starting…", "Finishing…", "Checking…". After save: "Start agent" or "Finish profile" |
| Extra | subtle | "Retry status" when control is in error |

Dismissal: backdrop and Escape close the dialog only when it is not dirty and not busy.

### 5a.8 Edit agent dialog and other agent dialogs

`src/bundled/agents/AgentEditor.tsx:96-393`:

- **Chrome:** `.buzz-dialog.agent-dialog.agent-editor`, width `min(40rem, 100vw-2rem)` (640px).
- **Header:** "Edit agent" + `IconButton size="compact"` with Tabler `x` 16px ("Close editor"). The description is sr-only.
- **Identity block:** `space-y-3 text-center`.
  - The `AvatarEditor` (squircle), or a 40px squircle `Avatar` when editing is unavailable.
  - The display name in `text-label`.
  - The relay URL in `text-body-sm text-subtle`.
- **Settings:** then `AgentSettingsFields` (same fields as 5a.7).
- **Accordion `form`, item "Runtime":**
  - process label;
  - mode line (`text-body-sm text-subtle`), e.g. "Enabled · starts with buzz-app" or "Manual start · a later sent mention can start this agent";
  - buttons "Start"/"Retry start", "Stop", "Restart"/"Restart to apply";
  - "Saved revision N · Running revision M." note.
- **Accordion `form`, item "Technical details":** a `dl` with "Public key (npub)" and "Relay" (`text-mono`, select-all), plus "Host diagnostics" (`pre text-mono`).
- **Footer:** "Cancel" (subtle), "Discard changes" (only when stale), "Save changes" (primary, disabled until dirty). Success fires a toast "Saved." or "Saved. Restarted N agents." (`src/features/agents/control.ts:162-169`).

| Dialog | Source | Title | Body | Buttons |
|---|---|---|---|---|
| Delete | `AgentDeleteDialog.tsx:50-90` | "Delete {name}?" | "This stops the agent and removes its settings and app credential from this device. Its relay identity and past messages remain visible. This cannot be undone here." | Cancel; **Delete agent** (destructive: `--affordance-danger` fill, `--text-danger` text) |
| Archive | `src/features/agents/AgentArchiveDialog.tsx` | "Archive {name}?" | "This hides {name} from search, mentions, and Add member in {community}. It won’t remove the agent from its channels or delete its saved setup. You can unarchive it later." (+ "This agent is running. Archiving won’t stop it.") | Cancel; **Archive agent** (prominent). On success: toast "{name} archived in {community}" with Undo |
| Remove (relay-only) | `RelayAgentRemove.tsx` | "Remove {name}?" | "Removing this agent cleans it up in this community. It does not delete the agent's key." + 3 bullets | Cancel; **Remove agent** (destructive) |
| Use here / Clone handover | `AgentControlPanel.tsx:465-520` | "Set up agent here" / "Review agent to clone" | "Set up the imported agent in this community. It will not start yet." / "Create a new agent from the saved name and instructions…" | Close |
| Export / share | `AgentSnapshots.tsx:223`, `DirectShare.tsx` | "Export {name}" | `<select>` "Agent only / Agent + core memory / Agent + all memories" | |
| Snapshot import | `AgentSnapshots.tsx:563` | "Import agent snapshot" / "Agent imported" | | |
| Community Catalog | `CommunityCatalog.tsx:545-620` | "Community Catalog" (wide 896px, stable height). Grid columns 14rem + 1fr, gap 24px | Empty: "Nothing shared yet" / "Shared agents and teams will appear here." | "Add agent" / "Added to My Agents" (prominent). The Agents page has no trigger that sets `catalogOpen` to true, so in practice catalog agents are reached through the Add agent sidebar |

### 5a.9 Inline status and alert copy (top of inventory)

These sources render bare `<p>`s with `role=status` or `role=alert`, at body text size:

- `AgentControlPanel.tsx:375-394`:
  - "Reading local agent status…"
  - `{state.error}`
  - "The agent statuses below may be out of date." (`text-body-sm text-secondary`)
  - a `Retry status` button
  - "Waiting for the desktop app…"
- `UnifiedInventory.tsx:342-405`:
  - "{warning} Retry local discovery by reopening the app."
  - "Checking community inventory…"
  - "Agent names and pictures could not be checked for {community}. Refresh to retry."
  - "Community inventory could not be checked for {community}. Refresh to retry."
  - "Showing known community associations. Community checks are not current."
- `inventory-decisions.ts:44-49`:
  - "Connect to a destination community to set up this identity."
  - "No import source has been confirmed. Check any discovery warnings."

### 5a.10 Empty states (exact copy)

| Where | Copy |
|---|---|
| Inventory with zero rows (`InventoryView.tsx:258`) | "No agents yet. Add an agent to get started." |
| All archived (`InventoryView.tsx:259`) | "All your agents are archived in this community." |
| Legacy managed grid (`AgentsPage.tsx:514-516`) | "No agents yet. Create an agent or import one from old Buzz below." |
| Old library (`AgentLibrary.tsx:111`) | "No visible identities in your Buzz library." |
| No host (`AgentsPage.tsx:468-471`) | "Open the desktop app to import and run agents. You can still mention existing channel members." |
| Card without identity | "No linked identity" |

None of these use the `.buzz-empty-state` component. They are plain body-text paragraphs.

The legacy managed grid header text (`AgentsPage.tsx:506-513`) is:

- `h2 text-label-sm` "Individual agents";
- the paragraph "Set up an imported agent with Use here, then start it separately. Before starting the same identity here, stop the old agent and disable its automatic startup in the old app."

The old library is a `<details>` with the summary "Other agents (N)".

### 5a.11 Settings → Agents (`src/app/AgentSettings.tsx`, `src/app/AgentDefaultsCard.tsx`, `src/app/AgentSettings.module.css`)

| Block | Spec |
|---|---|
| Header | `Header title="Agents"` |
| "Harnesses" `InlineHeader` actions | `IconButton sm` Tabler `help-circle` 16px ("About ACP", tooltip with the ACP hint); `IconButton sm ghost` Tabler `refresh` ("Check again", spinner while checking) |
| Harness rows | `ul.rows` (margin 12px 0; 1px `--border-standard` between `li`). Each `li` is `py-3 text-body-sm` (12px vertical): 32px logo + label on the left (gap 12px); status ("Ready" / "CLI needed" / "Adapter needed" / "Unknown", `text-secondary`) + optional "Install"/"Update Pi" `sm` button on the right. Logos: buzz-agent = Tabler `robot-face` 32; goose = `GooseLogoIcon` 32; hermes = `HermesLogoIcon` 32; pi = `PiLogoIcon` 32; other = Tabler `terminal-2` 32 |
| Pi / preset setup | `.piSetup`: margin-top 12px, margin-left 8px, padding-left 16px, 1px left border. `<details>` "Manual setup" / "Manual {label} setup". Command rows use a copy `IconButton` (Tabler `copy`) |
| Add harness | `Button outline sm` with Tabler `plus` 16 + "Add harness". The dialog is "Add harness" (wide, stable height): grid columns 12rem + 1fr, gap 24px. Sidebar items use a 24px Hermes logo or `terminal-2`. The detail pane has padding 24px, radius 12px, `--surface-inset`, and a 48px logo + `text-heading` name. Action: prominent "Setup guide" with Tabler `external-link` 18 |
| Agent defaults (`AgentDefaultsCard.tsx:635-890`) | `InlineHeader "Agent defaults"`, subtitle "Used when an agent starts without its own choices. Changes apply on its next start." Fields in order: "Default harness" (Select: Buzz Agent / Goose / Pi); "Default provider" ("Use build default ({x})" or "Not set (use harness default)", providers, "Custom ID"); "{Label} API key"; "Search models" (only if >10); "Default model" + "Browse models" / "Retry models" / "Cancel model lookup"; "Default effort" ("Not set (use harness default)", then buzz-agent: none / minimal / low / medium / high / Extra high / max, goose: off / low / medium / high / max, pi: off / minimal / low / medium / high / Extra high); "Conversation context" (Entire channel / Each thread); "Environment variables" fieldset (rows `py-2`, key in mono, state "Set" / "Set on save" / "Removed on save", Keep/Remove `sm` button; Accordion "Add environment variable" with Name / Value fields and "Add variable"). Footer `justify-end gap-3 pt-2`: "Discard", **"Save defaults"** (primary) |
| Messages | `SwitchPreferenceRow` "Remember mentioned agents" – "Keep the same agents selected for your next message in this channel or thread." |

### 5a.12 Representative data (fixtures)

- `src/features/agents/control-testing.ts` (`controlFixture`) defines one agent:
  - name "Fixture agent", relay `wss://relay.example.test`, workspace `/fixture/workspace`;
  - command `fixture-acp`, args `["--literal","two words","quoted \"value\""]`;
  - model `fixture-model`, provider `fixture-provider`, status `running`.
  - Harness options: "Buzz Agent" (`buzz-agent`, providers `[{value:"databricks_v2",label:"Databricks v2"}]`).
- `tests/fixtures/agent-control.tsx` marks the agent `startOnAppLaunch=false`. Its model catalog is `{id:"catalog.schema.real-model",name:"Friendly Model"}` and `{id:"endpoint-two",name:"Other Model"}`, and "many" mode adds "Catalog Model 3…20".
- `tests/fixtures/agents.tsx` contains the library definitions "{scope} Brain" (with a 1x1 PNG data-URI avatar) and identities "{scope} Brain identity".
- Harness presets (`crates/agent-controller/src/harness-presets.json`) are "Hermes Agent" (`hermes-acp`) and "Claude Code" (`claude-agent-acp`).

## 5b. Settings shell

Sources: `src/app/Settings.tsx`, `src/app/Settings.module.css`, `src/app/SettingsSidebar.tsx`,
`src/app/settings-sections.ts`, `src/app/App.tsx:150-185, 269-292`, and the
`*Settings.tsx` files listed in 5b.5.

### 5b.1 Presentation: a page, not a modal

- Settings is a **shell destination** (`selected === "settings"`). The header, rail and
  joined Panel stay in place. The left sidebar region is replaced by `SettingsSidebar`,
  and the main region renders `Settings` with `navigationPane`. That mode hides
  Settings' own internal navigation column (`Settings.module.css:112-117`).
- Width: the Settings sidebar uses the saved channel-sidebar width (default 260, min 220).
  It has no resize handle. It collapses with the header sidebar toggle (2.3, 2.8). At
  <=650px it becomes the 220px drawer.
- Entry: profile menu > Settings, or the community rail menu > Community settings or
  Invite. **Back** (first row) returns to the previous view.

### 5b.2 Settings sidebar

`SettingsSidebar.tsx:56-94`, `Settings.module.css:84-110`. It reuses the channel
sidebar's `.sidebar` class, so the row recipe in 3.3 applies: 28px rows, padding 2/6/2/8,
radius 8, 14px text in `text-navigation`, hover `#f7f7f7`, selected `#e8e8e8`. Rows are
**not** inside the channel list, so they have no 4/10px fill inset and span the full
padded width.

| Part | Value |
|---|---|
| Container | `padding-top: 8px`, `padding-inline: 8px`, `overflow-y: auto`, `text-body-sm` |
| Back row | `NavigationItem` label "Back", `IconArrowLeft` 18px |
| Nav | flex column, gap 16px, padding 12px top / 8px bottom |
| Group title | `h2`, `text-caption` 12px/16px, color `text-metadata` (light `#5f5f5f`, dark `#a4a4a4`), `margin: 0 0 4px`, `padding-inline: 8px`, not uppercased |
| Group items | flex column, gap 2px |
| Item icons | 18px. Contributed cards without an icon use `IconMessageCircle` |

**Nav items in order**. Built by `settingsSections()`, `settings-sections.ts:156-221`.
Contributed cards appear in plugin registration order. The registration order of the
bundled plugins is `src/bundled/index.ts:60-140`.

| Group (title) | Items (label: Tabler icon) | Condition |
|---|---|---|
| *Selected community name* | Profile: `IconUser`; Templates: `IconCopy` (plugin off by default); Custom emoji: `IconMoodSmile` (Personal groups were removed in buzz-app #988) | only when a community is selected |
| Administration | Membership: `IconUsers` | community selected and the viewer is owner/admin (card visibility) |
| Integrations | Builderlab: `IconMessageCircle` (no icon declared) | Builderlab plugin enabled |
| Account | Pair mobile: `IconMessageCircle` | Pairing plugin enabled |
| Communities | Hosted communities: `IconWorld` | Hosted plugin enabled |
| App | (Profile: `IconUser`, only when **no** community is selected), Appearance: `IconPalette`, Notifications: `IconBell`, Shortcuts: `IconKeyboard`, Agents: `IconRobotFace`, Plugins: `IconLayoutGrid`, Updates: `IconDownload` | always |
| Development | Developer: `IconTool` | dev server on localhost only |

Default section: Profile when a community is selected, otherwise Appearance.

### 5b.3 Detail pane

`Settings.module.css:3-47`; `Settings.tsx:100-140`.
- The Settings root is a nested Panel inside the shell Panel. It is flush (no border or
  radius) and scrolls vertically.
- `.detail`: padding **32px top, 24px left/right, 24px bottom**,
  `scroll-padding-block: 24px`, `container-type: inline-size`.
- Each direct child is capped at **768px** (`max-inline-size: 48rem`) and centered.
- At a container width of 576px (36rem) or less, the standalone layout stacks.
  `.detail` padding becomes 32px / 16px / 64px bottom.
- A visually hidden `h1` "Settings" is always present. Each section starts with a
  `Header` (`h2`).

### 5b.4 Shared settings components

| Component | Visual | Source |
|---|---|---|
| Section title `Header` | Title `text-heading` 24px/500 (26.4px line), margin-bottom **32px**. Subtitle `text-caption` 12px `text-subtle`, 8px below the title. Horizontal padding 17px (group inset 16 + 1) so text aligns with the group content. Optional trailing actions | `settings-group.css:6-31`, `content-header.css`, `ui/Header.tsx` |
| Subsection `InlineHeader` (`h3`) | Title `text-label-sm` 14px/500, margin-bottom 12px, caption subtitle, optional actions on the right | `content-header.css:31-36` |
| `SettingsGroup` (the "card") | **Outlined, no fill**: `border: 1px solid border-standard`, `radius 16px` (`radius-container`), `padding: 0 16px`. Layout `rows`: a 1px `border-standard` divider between direct children. Layout `form`: grid with 16px gap and 16px vertical padding. Inset drops to 12px below a 384px container and 4px below 128px | `settings-group.css:38-71` |
| `PreferenceRow` | Grid `[icon?] [label + description] [trailing]`, gap 16px, padding **12px 0**, `align-items: center`. Icon box 20x20 `text-subtle`. Label `text-label-sm` 14px/500 `text-standard`. Description `text-caption` 12px `text-subtle`, 2px below the label (inside a settings group). Trailing: flex, right-aligned, wraps, gap 8px. Below a 384px container, wide trailing controls drop to their own line | `components.css:1530-1600`, `ui/PreferenceRow.tsx` |
| `Switch` | Track 36x20px, `radius-pill`, 2px padding, 1px `border-standard`, fill `affordance-subtle-hover` (`#f1f1f2`). Checked: border and fill `affordance-prominent` (black in light, white in dark). Thumb 14x14 `text-inverse` with `shadow-xs`, moves 16px over 220ms `cubic-bezier(.645,.045,.355,1)`. Disabled: fill `affordance-selected`, thumb `affordance-subtle-pressed` | `components.css:1475-1527` |
| Buttons | Settings sets `--radius-capsule: radius-control` (12px), so buttons in Settings have 12px corners, not pill corners | `Settings.module.css:4` |
| Spacing between subsections | 32px (`gap-section-gap` or `mt-section-gap`) | `tokens.css:729` |

### 5b.5 Sections and their fields (in order)

1. **Profile** (`src/app/ProfileSettings.tsx:156-420`)
   - Header "Profile". Subtitle: "Update this community’s profile. Other community
     profiles stay unchanged." or, with no community, "Used for new communities.
     Existing community profiles stay unchanged."
   - Form group:
     - preview row: compact `AvatarEditor` and the name (`text-label-sm`, "Your profile"
       when empty), with the about text below
     - fields "Display name" (max 100) and "Profile description (optional)" (textarea)
       (`src/features/communities/ProfileFields.tsx:52-73`)
     - when there are changes: **Cancel** and **Save** (primary), right-aligned, 24px
       above, gap 12px
   - InlineHeader "Identity details" ("Your identity is shared across communities.
     Public keys are safe to share."). Rows group with "Public key (hex)" and "Nostr
     address (npub)". Each value is in mono 12px with a 32px ghost copy button
     (`IconCopy`).
   - Form group: private key widget.
   - Rows group: "Sign out of Buzz", with the description "Remove your private key from
     this device. Also ends Builderlab and hosted community logins." and a "Sign out of
     Buzz" button.
2. **Appearance** (`src/app/AppearanceSettings.tsx`)
   - Header "Appearance". One rows group:
     - "Color mode": `ThemePicker`. Three thumbnails (System, Light, Dark), each 80px
       wide with a 10:7 aspect, radius 8, 1px outline offset 2. The selected one has a
       2px `border-focus` outline. Gap 12px (`settings-controls.css:1-70`).
     - "Interface size": a "−" button, the percentage (`text-body-sm`, tabular numbers),
       a "+" button, and when not at 100%, a reset IconButton (`IconRotate`). Gap 12px.
       Range 80%–200%.
3. **Notifications** (`src/app/NotificationSettings.tsx:160-290`)
   - Header "Notifications". Subsections are 32px apart.
   - InlineHeader "Notify me about": switch rows Mentions, Direct messages, Thread
     replies.
   - InlineHeader "Delivery":
     - "Desktop alerts" switch
     - a permission callout: 12px padding, radius 12, `surface-inset` fill, or the amber
       warning fill, with "Allow notifications" / "Check permission" link buttons
     - "Notify while viewing" switch ("Include direct messages in the conversation
       you’re viewing.")
     - "Sound" switch ("Choose a sound to preview it."), and when on, compact selects for
       "Direct messages", "@Mentions" and "Thread replies"
   - InlineHeader "Dock unread badge" (macOS only).
4. **Shortcuts** (`src/app/ShortcutSettings.tsx:266-300, 365-450`)
   - Header "Shortcuts". One InlineHeader and rows group per owner: "Buzz" first, then
     each plugin by name.
   - Each row: the title as an `h4` (`text-label-sm`); a subtitle of "Reserved by Buzz",
     "Modified" or "Also used by …"; then trailing:
     - the key combo chip (`body-sm`, padding 4x8, pill, `surface-inset`, `text-subtle`)
     - a 32px ghost edit button (`IconPencil`, or `IconX` while capturing)
     - "Reset" when modified
5. **Agents** (`src/app/AgentSettings.tsx:141-540`)
   - Header "Agents".
   - InlineHeader "Harnesses", with actions: About ACP (`IconHelpCircle`) and Check again
     (`IconRefresh`). Form group listing the harnesses: 32px logo, label, status, and an
     install/update button, rows 12px vertical. Then an "Add harness" outline button.
   - "Agent defaults" (`src/app/AgentDefaultsCard.tsx:636-830`): Default harness,
     provider, model, Default effort, Conversation context, Environment variables.
   - "Saved agent activity · <community>" (`src/app/ArchiveSettings.tsx`): Save agent
     activity, Save agent turn metrics, Activity retention.
   - InlineHeader "Messages": switch "Remember mentioned agents".
6. **Plugins** (`src/app/Settings.tsx:225-365`)
   - Header "Plugins". Subtitle on web: "Open the desktop app to load plugins from a
     folder or Git repository."
   - Folder/Git import area.
   - Rows group, one row per plugin: `IconLayoutGrid` 20px, the plugin name, and a
     switch. Channels shows "Required" instead of a switch. Roll back / Reload / Delete
     buttons appear when they apply.
7. **Updates** (`src/features/updates/UpdateSettings.tsx:67-95`)
   - Header "Software updates".
   - An `EmptyState` with `IconRefresh`, "Version x.y.z", the status text, and a button
     (Check for updates / Update now / Retry / Check again).
8. **Developer** (dev only, `src/app/DeveloperSettings.tsx:105-207`)
   - Header "Developer" ("Activity and diagnostics for this session.").
   - Broker activity.
   - Runtime settings: Log level.
   - Caches.
9. **Plugin cards** (Membership, Custom emoji, Templates, Builderlab,
   Pair mobile, Hosted communities) render their own components in the same detail pane.
   They use the same `Header`/`SettingsGroup` primitives. Their contents are outside the
   scope of this section.

## 6. Component visuals

`data-buzz-ui` roots default to `text-body` (14/20/400, −0.005em) in Inter, colored `text-standard`.
"Hover" applies only under `(hover: hover) and (pointer: fine)` unless noted. "Pressed" also covers `aria-expanded="true"`.
Ctx = interaction context (1.4).

### 6.1 Button

Source: `DS/ui/Button.tsx`, `DS/styles/components.css:9-176,295-315`.

**Geometry**:

| Size | min-height | Padding (block / inline) | Font | Icon / spinner |
|---|---|---|---|---|
| `xs` | 24 | 4 / 12 | caption 12/16 400 | 16 |
| `sm` (alias `compact`) | 32 | 6 / 16 | label-sm 14/20 500 | 16 |
| `md` (default, alias `default`) | 40 | 8 / 24 | label-sm | 24 |
| `lg` | 52 | 14 / 24 | label-sm | 24 |

Common to all sizes:
- Radius `--radius-capsule` 26px, which gives a full pill at ≤52px.
- `shape="control"` uses a 12px radius. Thread-summary buttons use 10px.
- Label is `inline-flex`, gap 8. The label is `nowrap` and the button never shrinks.
- Default variant is `subtle`. Aliases: `primary` → prominent, `quiet` → subtle.

**Variant colors**:

| Variant | Rest bg / text | Hover | Pressed / expanded |
|---|---|---|---|
| prominent | `#000000` / `#ffffff` (D `#ffffff` / `#000000`) | `#191919` (D `#c7c7c7`) | `#525252` (D `#b8b8b8`) |
| subtle | ctx fill (panel L `#f5f5f6`, D `#232323`; floating D `#404040`) / text-standard | ctx hover (panel `#f1f1f2` / `#2e2e2e`; floating D `#595959`) | ctx pressed (`#dadada` / `#333333`; floating D `#737373`) |
| ghost | transparent / text-standard | ctx hover | ctx pressed |
| outline | transparent + `box-shadow: inset 0 0 0 1px` ctx border (L `#808080`, D `#737373`; floating D `#808080`) | ctx hover fill (stroke kept) | ctx pressed |
| inverted | `surface-panel` (`#fff` / `#1a1a1a`) | ctx hover | ctx pressed |
| destructive | `#feebec` / `#641723` (D `#3b1219` / `#ffd1d9`) | `#ffdbdc` (D `#500f1c`) | `#ffcdce` (D `#611623`) |
| link | transparent, no underline | underline, offset 4px | underline |

**States**:
- Transitions: `background-color, color 150ms ease`.
- Disabled (not loading): text `#959595` (D `#595959`), bg `affordance-disabled` `#e8e8e8` (D `#1a1a1a`), shadow none, `cursor: not-allowed`. Ghost, outline, and link stay transparent. Outline disabled keeps an inset 1px `border-standard` stroke (`#e8e8e8` / `#232323`).
- Loading: the label gets `opacity:0` but keeps its geometry. A `CircleNotch` spinner (16/24px) is centered on top and spins 1s linear. Variant colors are kept and `cursor: progress` is set.
- Focus: 2px `text-standard` ring with offset 2.
- Top-bar context: hover and pressed fills come from the backdrop translucents (1.4).
- A `title` prop wraps the button in the shared Tooltip (6.10).

### 6.2 IconButton

Source: `DS/ui/IconButton.tsx`, `components.css:178-293`, `shared/styles/globals.css:336-353`.

Square, padding 0, radius 10px (`--radius-icon-button`). Default variant is `ghost`, default size is `md`.

| Size | Box | Icon |
|---|---|---|
| `xs` | 20×20 | 12 |
| `sm` (alias `compact`, `toolbar`) | 32×32 | 16 (`bare`: 20) |
| `md` | 40×40 | 24 |
| `lg` (alias `large`) | 52×52 | 24 |

The CSS forces the icon size regardless of the icon's own `size` prop. Exception: navigation-tab close buttons are `xs` boxes with 16px art. In the shell header, every IconButton is 32×32 with 16px art.

| Shape | Geometry |
|---|---|
| `control` (default) | 10px radius |
| `round` | pill (default for `avatar`) |
| `row-end` | `align-self: stretch`, radius `0 8 8 0`. Hover uses ctx row-hover. Two 8×8 inverse-corner pseudo-elements sit at the left edge, masked by `radial-gradient(circle at bottom-left / top-left, transparent 8px, black 8px)`, and inherit the fill. |

**Variants** (fills follow 6.1):

| Variant | Behaviour |
|---|---|
| prominent / solid, subtle, outline, destructive, inverted | As in 6.1 |
| `ghost` | Transparent at rest. Hover and pressed use ctx fills. `data-selected` uses ctx selected (panel `#e8e8e8` / `#333333`) and keeps that fill on hover. |
| `tint` | `#f7edfe` with `#402060` icon (D `#301c3b` / `#ecd9fa`). Hover and pressed: `#f2e2fc` (D `#3d224e`). |
| `chrome` | `glass-primary` (glass-2 → glass-3 on hover), blur 16, rim shadows. `aria-current="page"` shows `bg-chrome-selected` (`#fff` / `#333333`) plus `shadow-xs`. |
| `media` | media-glass, static on hover and press |
| `avatar` | No fill in any state (the backdrop shows through avatar cutouts) |
| `bare` | Always transparent. The glyph carries the state. |

Disabled ghost stays unfilled with an `#959595` / `#595959` glyph. Tint and chrome keep their fills.

### 6.3 Input, Textarea, InputGroup, SearchField

Source: `DS/styles/forms.css:1-104,289-357`, `DS/styles/search-field.css`, `DS/ui/SearchField.tsx`.

| Part | Spec |
|---|---|
| Input (md) | `text-body` 14/20. min-height 40. Padding block `max(4, (40 − 20 − 2)/2)` = **9px**, inline 16. Radius 12. Bg `surface-inset` L `#f5f5f6`, D `#101010`. Text `text-standard`. |
| Input `sm` | `text-body-sm`, min-height 32, padding block 5 |
| Border states | Always reserves `1px solid var(--border)`. Rest = transparent. Focus (pointer) or `data-popup-open` = ctx border (L `#808080`, D `#737373` on panel / `#808080` floating). Invalid = `#e5484d` (both modes) and wins over focus. Transition `border-color 150ms ease`, instant under keyboard navigation. |
| Placeholder | `text-metadata` L `#5f5f5f`, D `#a4a4a4` |
| Disabled | text `#959595` / `#595959`, bg `#e8e8e8` / `#1a1a1a`, `not-allowed` |
| Textarea | Same as Input, plus min-height 96, padding 16 on all sides, vertical resize. Resize grip: two diagonal `text-metadata` lines in an 8×8 box, inset 4px from the corner. `variant="code"` uses `text-mono` 12/16. |
| Field | Grid with gap 8. Label `text-label-sm` text-standard. Description `text-body-sm` text-subtle. Error `text-body-sm` text-danger (replaces the description). Form field-to-field gap is 16, section gap 32. |
| InputGroup | flex, gap 8, min-height 40, padding-inline 16. A trailing action sets end padding to 3px; a leading button sets start padding to 3px. Radius 12, bg surface-inset, icon color text-subtle. The same `--border` states apply on `:focus-within`. The trailing slot is 32px wide. The inner input is borderless and transparent. Disabled uses the disabled bg and text. |
| SearchField default | InputGroup with a leading MagnifyingGlass (16) and a trailing clear IconButton (`sm` ghost, X 16) |
| SearchField `capsule` (mention/media pickers, Emoji Mart) | min-height 40, gap 8. Border `1px solid border-prominent` (`#808080` / `#737373`). **Pill** radius, padding 0 16 (right 4 with the clear button). Bg `surface-panel`. Icon/text color `text-metadata`; input text `text-standard`. `text-body-sm`. Clear button 32×32, radius 10, `text-metadata`; hover `#f1f1f2` / `#2e2e2e`, active `#dadada` / `#333333`. Disabled = opacity .5. Invalid border = `#e5484d`. No focus style. |

### 6.4 Select and Combobox

Source: `DS/ui/Select.tsx`, `forms.css:178-287`, `components.css:1360-1425`.

**Triggers**:

| Variant | Trigger |
|---|---|
| `field` | Input recipe (6.3) laid out flex, space-between, gap 8. CaretDown 16 on the right. Placeholder uses `text-metadata`. |
| `inline` | Label (`text-primary`) followed by a ghost md Button with a CaretDown 14 |
| `compact` | Outline sm Button, value ellipsized, CaretDown 14, label visually hidden |

The chevron rotates 180° while open (150ms settle; instant under keyboard navigation).

**Popup**:
- `floating-surface`: radius 16, 1px border-standard, glass `rgba(255,255,255,.9)` / `rgba(40,40,40,.9)` + blur 8, shadow-sm.
- Padding 4. `min-width` = trigger width (`field`: width = trigger). `compact`: `max(trigger, 180px)`. `max-width` = `100vw − 24px`.
- `sideOffset` 4. Motion as in 1.7.

**Option**:
- min-height 32, padding 8 12, radius 11, flex space-between, gap 16, text-standard.
- Highlighted: ctx row-hover (floating L `#f5f5f6`, D `#333333`); supporting text becomes text-standard.
- Selected: a trailing Check (14) is shown. The mark slot is always reserved (`visibility: hidden` when not selected).
- Disabled: `text-unavailable`.
- Group label: padding 8 12 4, `text-secondary`.

**Combobox**:
- Input with a trailing `sm` IconButton holding a CaretDown 16, or CircleNotch spinning while loading.
- The popup reuses the select popup.
- Options align to the start. Description `text-body-sm text-subtle`. Check 16 with 4px top padding.
- Empty state: padding 8 12, text-subtle.

### 6.5 Switch, Checkbox, Radio

Sources: `components.css:1474-1528`, `forms.css:105-175`.

**Switch** (border-box):

| Part | Spec |
|---|---|
| Track | 36×20, padding 2, border `1px border-standard` (`#e8e8e8` / `#232323`), pill. Bg `affordance-subtle-hover` `#f1f1f2` / `#2e2e2e`. |
| Thumb | 14×14 pill, bg `text-inverse` (**L `#ffffff`, D `#000000`**), `shadow-xs` |
| Checked | Track bg and border `#000000` (D `#ffffff`). Thumb `translateX(16px)`. |
| Motion | Bg and border 150ms ease; thumb 220ms settle |
| Disabled | Track bg `affordance-selected` `#e8e8e8` / `#333333`. Thumb `#dadada` / `#333333`, no shadow. Label `text-unavailable`. |
| Label | Wrapper gap 12, `text-body` |

**Checkbox**:

| Part | Spec |
|---|---|
| Box | 20×20, radius 4. Border `1px` ctx border (`#808080` / `#737373`). Bg `surface-panel`. |
| Checked / indeterminate | Bg and border `#000` / `#fff`. Check or Minus glyph 14px in `text-inverse`. |
| Disabled | Bg `#e8e8e8` / `#1a1a1a`, glyph `text-unavailable` |
| Choice row | gap 12, min-height 40. Label `text-label-sm`, description `text-body-sm` subtle. |

**Radio**:
- Same as Checkbox but pill-shaped. Checked shows an 8px `text-inverse` dot on the prominent fill.
- Group gap 12.
- `variant="card"`: padding 16, 1px border-standard, radius 12. Checked: ctx border, ctx selected bg, and text-subtle → selected-text.

### 6.6 Dialog and AlertDialog

Source: `DS/ui/Dialog.tsx`, `DS/ui/AlertDialog.tsx`, `DS/styles/overlays.css:1-235,309-316`.

| Part | Spec |
|---|---|
| Scrim | Fixed, full screen. L `rgba(0,0,0,0.376)`, D `rgba(0,0,0,0.651)`. Fades 150ms in / 120ms out. z 1000. |
| Popup | Centered with `translate(-50%,-50%)`, z 1001. Width `min(496px, 100vw − 32px)`, max-height `100dvh − 48px`, overflow auto. Border 1px border-standard, radius 20, padding 24, shadow-sm, elevated-material (glass `rgba(255,255,255,.9)` / `rgba(40,40,40,.9)` + blur 8; opaque `#fff` / `#282828`). Uses the floating interaction context. Text `text-standard`. |
| Sizes | `wide`: `min(896px, 100vw − 32px)`. `expanded`: `100vw − 48` × `100dvh − 48`. `placement="right"`: right-anchored sheet, `min(416px, 100vw)` × `100dvh`, radius 0, no transform. `height="stable"`: `min(576px, 100dvh − 48px)`; the body flexes and scrolls. |
| Layout | Flex column with **gap 0** (`gap-0`). Header → body → footer. |
| Header | flex, align start, space-between, gap 16. Heading column gap 8. Title `text-label` 16/24/500. Description `text-body-sm` text-subtle. Header actions gap 8, ending with Close: IconButton `sm` ghost (32×32, radius 10) with X 16. |
| Body | `padding-block: 24px`, so header→body and body→footer are both 24. `headerGap="compact"` sets top padding 12. `footerGap="compact"` sets bottom padding 8. |
| Footer | flex wrap, `justify-content: safe end`, gap 12, overflow-x auto. Leading actions use `margin-right: auto`. Cancel comes before the primary action. |
| ≤480px | Padding 16 everywhere (body padding too). Footer buttons `flex: 1 0 auto`. |
| Motion | Opacity 150ms ease and scale .98→1 over 220ms settle. Exit 120ms ease. Escape close and keyboard navigation are instant. `motion="none"` for the search palette. |
| AlertDialog | Same popup and backdrop classes, but its flex column **gap is 16**. Title `text-heading` 24/1.1/500. Description, then children, then actions. No motion (no `data-motion`). No close button. |

### 6.7 Menu

Source: `DS/ui/Menu.tsx`, `components.css:1085-1191,1205-1334`.

| Part | Default / wide | Compact |
|---|---|---|
| Popup | floating-surface. Radius 16, padding 4, `text-body-sm`. min-width 184. max-width `min(320, available, 100vw − 16)` (wide: 448). max-height `min(384, available)`, scrolls. | Radius 8, padding 2 |
| Item | min-height 32, padding 8 12, radius 11, gap 8, text-standard, no transition | Padding 4 8, radius 5 |
| Highlight (hover/keyboard) and open submenu parent | Floating row-hover L `#f5f5f6`, D `#333333`. Icons and supporting text turn text-standard. | `#f1f1f2` / `#2e2e2e` |
| `selection="highlight"` checked radio | `#f5f5f6` / `#404040` (popover-selected), no check mark | same |

Other parts:
- Leading icon: 16×16, `text-subtle`.
- Trailing check: 16px, slot always reserved.
- Submenu arrow: CaretRight 16, `text-subtle`, pushed to the end.
- `tone="danger"`: `text-danger` (`#641723` / `#ffd1d9`).
- Disabled: `text-unavailable`.
- Group label: padding 8 8 4, `text-subtle` (`emphasis="quiet"`: `text-metadata`).
- Separator: 1px `border-standard`, margin 4 0.
- MenuNote: padding 8, `text-body-sm` text-subtle.
- Positioning: `side=bottom align=start sideOffset=4 collisionPadding=8`.
- Motion: 75ms in / 60ms out with a 2px offset from the placement side and 2px blur.

### 6.8 Popover and PreviewCard

Source: `DS/ui/Popover.tsx`, `DS/ui/PreviewCard.tsx`, `components.css:317-390,1192-1246`.

**Popover**:
- floating-surface, radius 16, `text-body`, color text-standard.
- Width 320 (compact 224, wide 384); max-width `min(available, 100vw − 16)`.
- `padding="content"` = **12px** (DESIGN.md says 16; the CSS uses 12). `list` = 4 (compact list = 2). `none` = 0.
- Compact: radius 8.
- Title `text-label`. Description `text-body-sm` text-subtle, margin-top 8.
- Positioning: `side=bottom align=start sideOffset=8 collisionPadding=8`. Motion as Menu.
- `popover-surface` utility (feature popups such as completions): same paint, z 1100, selection → popover-selected.

**PreviewCard**:
- floating-surface, padding 16, grid gap 4, max-width `min(288px, 75vw)`.
- Opens after a 250ms delay (0 when it has an action) and closes after 150ms. `sideOffset` 8, align start.
- Fade + scale .98, 150ms ease.
- Content: kind (`text-body-sm` tertiary), name (`text-body` primary, ellipsis), status (`text-body-sm` secondary).

### 6.9 Toast

Source: `DS/ui/Toast.tsx`, `overlays.css:328-404`.

| Part | Spec |
|---|---|
| Viewport | Fixed. top `128px + safe-area` (≤700px: 192px). right `max(16, safe-area)`. Width `min(384, 100vw − 32)`. Column with gap 12, padding 4. max-height `min(40dvh, 100dvh − top − min(160px, 25dvh))`, scrolls. z 999. |
| Card | `floating-surface` (radius 16, border, glass, shadow-sm), padding 16, grid gap 8, fade 150ms |
| Heading | flex, space-between, gap 8. Title `text-label-sm`: error → text-danger, warning → text-warning, info/success → text-standard. The close button (when dismissible) is an `sm` ghost IconButton (32×32) with X 16. |
| Body | Description `text-body-sm`. Actions flex wrap, gap 8 (typically `sm` Buttons). |
| Timing | `useToastNotification`: 4000ms, or 8000ms with an action. `ToastNotice` default timeout 0 (persistent recovery). Dismissible toasts swipe right. |

### 6.10 Tooltip

Source: `DS/ui/Tooltip.tsx`, `overlays.css:236-277`.

| Part | Spec |
|---|---|
| Delay | Open delay **0ms** by default (callers may override: message timestamp 500, GitHub checks 300). Close delay 0. Closes on click. |
| Position | `side="top"` (or `right`), offset 4px, z 1200 |
| Box | `text-caption` 12/16. Padding 4 8. Radius `(1lh + 8)/2` = **12px** (pill for one line). Bg `affordance-prominent` L `#000000` / D `#ffffff`. Text `text-inverse` L `#ffffff` / D `#000000`. `shadow-sm`. max-width `min(384, 100vw − 32)`. Wraps anywhere. |
| Motion | Enter 150ms: opacity `ease`, transform settle, from `translateY(2px) scale(.97)` + `blur(2px)`. Exit 120ms. Instant under keyboard navigation. Reduced motion keeps only the fade. |

### 6.11 Tabs and NavigationItem

Source: `DS/ui/Tabs.tsx`, `DS/ui/NavigationItem.tsx`, `components.css:535-1074,1644-1707`, `overlays.css:291-308`.

**NavigationItem (`row`)**:
- min-height 32, padding 4 16, gap 8, radius 8, text-standard.
- Transition: color and background 150ms.
- Hover: ctx row-hover (panel L `#f5f5f6`, D `#232323`); subtle and metadata text use row-text/row-metadata.
- Selected: ctx selected (`#e8e8e8` / `#333333`).
- Active (pressed): ctx pressed (`#dadada` / `#333333`).
- Label ellipsis. Trailing slot `text-metadata`.
- `inset`: left padding 32.
- `option` variant: padding 8, no transition. Focus-visible fills `#f5f5f6` / `#333333` with an inset ring.
- `pill` variant: pill radius, padding-inline 16. Selected = `surface-panel`.

**Tab variants**:

| Variant | Container | Tab | Hover | Selected |
|---|---|---|---|---|
| `chrome` (default; on gradient) | glass-primary pill, height 40, padding 4, gap 4 | Height 32, padding 0 16, pill, `text-subtle` | glass-3 fill (`rgba(255,255,255,.62)` / `rgba(28,28,28,.659)`), text-standard | text-standard. Sliding indicator fills the tab box: pill, `#ffffff` / `#333333`, shadow-xs. Width and transform animate 220ms settle. |
| `panel` (on plain surface) | flex, gap 16. List gap 20, padding 4 (negative margin) | Height 36, padding 0, `text-metadata` | text-standard | text-standard. Indicator: 2px bar, pill, `text-standard` color, `bottom: 4px`, slides 220ms. No weight change. |
| `workspace` | List gap 8, wraps | Padding 4 12, pill, `text-subtle` | `#e8e8e8` / `#333333`, text-standard | text-standard, no indicator. Preview tab: `#f7edfe` / `#301c3b`. Arrival flash: same accent fill at opacity 1 for 0–30%, fading to 0 over 660ms ease-out. |
| `navigation` (pane header tab strip) | `text-body-sm`. List height 40, padding-top 4, gap 4, horizontal scroll | Each item is a fixed 192px (12rem) NavigationItem, radius 10, padding-inline 8 (32 at the end when closable), label ellipsis, 16px icon slot | ctx row-hover | `affordance-subtle` `#f5f5f6` / `#232323`. `showSelection=false` gives a transparent fill and default cursor. |

Navigation tab close button:
- `xs` IconButton (20×20), X at 16px, absolutely placed at inline-end 4px and vertically centered.
- On inactive tabs it is hidden (`opacity: 0`) unless the tab is hovered or has focus-within. It is always visible on touch devices.

Chrome route nav (`nav.chrome-navigation`):
- Same glass pill (height 40, padding 4, gap 4).
- Pills are 32 tall, padding 0 16, `text-label` 16/24/500, `text-subtle`.
- Hover: glass-3 fill. Selected: `bg-chrome-selected` + shadow-xs.

### 6.12 Panel chrome and content headers

Sources: `components.css:494-514,744-800,1035-1058`, `content-header.css`, `settings-group.css`, `empty-state.css`, `settings-controls.css:86-96`, `composer.css`.

| Component | Spec |
|---|---|
| PanelHeader | min-height 40, padding-inline 4, gap 8, flex wrap. Bottom hairline: 1px `border-standard`, inset 4px from each side (0 inside joined panels). Title `h2` `text-label`. Actions gap 4 (32px ghost IconButtons with 16px art). |
| PanelHeaderLabel | `text-body-sm` (regular), padding-inline 8, gap 8, icon slot 16×16 |
| Header (content) | flex, gap 12, margin-bottom 20 (inline variant: 12). Title `text-label` (inline: `text-label-sm`; Settings page: `text-heading`). Subtitle `text-body-sm` subtle, margin-top 8. Eyebrow `text-label-sm` metadata. |
| SettingsGroup | Border 1px border-standard, radius 16, padding-inline 16 (12 below a 384px container, 4 below 128px). `rows` layout uses 1px border-standard between rows. `form` layout uses a grid with gap 16 and padding-block 16. |
| PreferenceRow | Grid `1fr auto` (with icon: `auto 1fr auto`), gap 16, padding 12 0. Icon 20 subtle. Label `text-label-sm`. Description `text-body-sm` subtle (`text-caption` in settings). Trailing gap 8. |
| EmptyState | Border 1px border-standard, radius 16, centered. Padding 32/24. Gap 16. Icon 32 subtle. Title `text-label-sm`. Description `text-body-sm` subtle, max 48ch. Actions gap 12. |
| KeyboardShortcut (`kbd`) | `text-body-sm`, padding 4 8, pill, bg surface-inset, text-subtle |
| Composer (DS shell) | Padding 16 10 10. Border 1px border-standard, radius 20, shadow-xs, bg surface-panel, gap 16. Focus-within: see 1.9. Error border `#e5484d`. Input `text-body`, min-height 24, max-height 192. Placeholder `text-subtle`. Context strip: `text-body-sm`, padding 8 10, radius 8, bg affordance-subtle, `text-secondary`. |
| Accordion | Trigger: padding 8, radius 8, gap 12. Hover `affordance-selected` (`#e8e8e8` / `#333333`). Chevron rotates 180° when expanded. Panel padding 8. |

### 6.13 Avatar and presence

Source: `DS/ui/Avatar.tsx`, `components.css:392-478`, `DS/styles/avatar-shape.css`, `DS/styles/*.svg`, `features/agents/AgentAvatar.tsx`, `DS/ui/agent-thinking/*`.

**Sizes**:

| Size | Box | Initial font |
|---|---|---|
| `small` | 24×24 | `text-caption` 12/16 |
| `default` | 32×32 | `text-body` 14/20 400 |
| `large` | 40×40 | `text-body` 14/20 400 |
| `fill` | 100% of parent | `text-body` 14/20 400 |

Other sizes (for example the 40px profile header) come from parent boxes using `fill`.

**Shape**: identity type determines shape. Humans use `circle`; agents use `squircle`.
- Circle: `border-radius: 50%`.
- Squircle: an SVG mask stretched to the box (viewBox 0 0 1 1):
  `M .5 0 C .93 0 1 .07 1 .5 C 1 .93 .93 1 .5 1 C .07 1 0 .93 0 .5 C 0 .07 .07 0 .5 0 Z`.
  These are four cubic segments with control points 0.07 in from each corner.

**Fallback (no image, or load failed)**:
- Bg `affordance-subtle` (L `#f5f5f6`, D `#232323`). Text `text-subtle` (L `#525252`, D `#c7c7c7`).
- Content is the **first Unicode code point of the trimmed name, uppercased**, or `?` if empty. Callers may pass an icon instead (Lightning 20, Robot 16, User 19).
- **There is no hash-based color algorithm.** All fallbacks share the same neutral fill.
- Exception: the top-bar profile button uses a white disc (`#ffffff`, both modes) with `#525252` text.

**Image behaviour**: `object-fit: cover`. Opacity is 0 until the image loads, then 1 (no fade). The fallback appears only if loading takes longer than 150ms, or if the image fails.

**Emoji avatars**:
- Profile images may be the emoji-avatar SVG data URL: 512×512, rect `rx=112` (squircle-ish) or `256` (circle), a solid fill, and an emoji at font-size 258 centered at y=56% (`features/communities/community-icon.ts`).
- Picker palette (`features/profiles/AvatarEditor.tsx:46-70`): `#FFFFFF #FFF4CC #FFE75C #FFB84D #FF8652 #F6534F #FF6B9A #FB60C4 #D66BFF #B141FF #7C5CFF #476CFF #3399FF #63C6F2 #41EBC1 #2ED3A2 #73EF75 #9FE870 #C7D36F #CCCCCC #8A8F98 #4B5563 #000000`. Default `#FFF4CC`.
- These render as ordinary images.

**Status badge** (`statusBadge="online"|"away"|"offline"`):

| Part | Spec |
|---|---|
| Dot geometry | Diameter = 25% of the avatar (6 / 8 / 10px at 24 / 32 / 40). Centered at (85%, 85%). |
| Artwork cutout | The artwork is masked by a cutout of radius 20% of the size, centered at (85%, 85%). This leaves a transparent gap of 7.5% of the size around the dot (3px at 40). Circle mask in a 40-unit viewBox: `M 39.271 25.349 C 38.844 26.891 36.913 26.379 35.335 26.112 A 8 8 0 0 0 26.112 35.335 C 26.379 36.913 26.891 38.844 25.349 39.271 A 20 20 0 1 1 39.271 25.349 Z`. Squircle mask: `DS/styles/avatar-status-squircle-cutout.svg` (an 8-unit squircle notch). The joins use tangent cubics. |
| Dot shape | Circle for human avatars. For squircles the dot itself is squircle-masked. |
| Online | Outer `#218358` (D `#3dd68c`), then a 1px inset inner fill `#2b9a66` (D `#33b074`) |
| Away | Solid `#ffba18` (D `#ffd60a`), no outline |
| Offline | Solid `#808080` (D `#a4a4a4`) |
| Unknown | No badge, full shape |

**Agent "available/thinking" badge** (squircle agents only, `ThinkingBadge`):
- When the agent is online or thinking, the static dot is replaced by an animated ink in an 88px canvas, scaled to the avatar.
- Ink: 22×22, radius 7, centered at (74.8, 74.8), bg `status-online` `#218358` / `#3dd68c`.
- When thinking, the ink morphs over 300ms (quint-out, computed in JS) into a capsule anchored bottom-center at (44, 88). Its fill becomes `affordance-prominent` (`#000` / `#fff`, 300ms `cubic-bezier(.22,1,.36,1)`). It shows three 7px `text-inverse` dots, gap 4, that pulse over 1.2s linear with 80ms/160ms stagger (opacity .4→1, translateY −2px at 20%).
- An SVG notch follows the ink. Its gap is `min(6.6, 264/size)` at rest. While thinking, the gap is scaled by `0.75 + 0.25·clamp((size−40)/40, 0, 1)`.
- Geometry is in `DS/ui/agent-thinking/badge-cutout-geometry.ts`.

### 6.14 Chips, mentions, links, badges

Sources: `DS/styles/chips.css`, `DS/ui/InlineChip.tsx`, `shared/InlineReference.module.css`, `bundled/channels/Channels.module.css:783-795`.

| Element | Spec |
|---|---|
| Inline chip (composer and message references) | `inline-flex`, max-width 256. Radius **4**. Padding-inline **3px**, no block padding. Font inherited, line-height 1.35, baseline-aligned. Bg `affordance-subtle` L `#f5f5f6` / D `#232323`. Text `text-standard`. Label = sigil + name (`@` person or agent, `#` channel), ellipsized. Message and link kinds have a leading icon (ChatCircle / Link) at 1.1em, margin-end 4px, nudged −0.05em. Not selectable. |
| Chip states | Hover, keyboard focus, or preview open: `#f1f1f2` / `#2e2e2e` (150ms). Focus ring 2px `border-focus` with offset 2. **Unresolved**: bg `surface-inset` (`#f5f5f6` / `#101010`), text `text-metadata`, inert. Qualifier suffix reveal: grid-column 0fr→1fr plus opacity, 220ms settle. |
| Inline link / mention in prose | Color `text-link` L `#0b5fa8`, D `#83c4ff`. No underline. Padding 2 with margin-inline −2 (the hover box sits outside the text flow). Radius 4. Hover bg `affordance-link-hover` L `#e6f4fe`, D `#0d2847`. Leading icon 1em, margin-right 3px, vertical-align −0.125em. Focus 2px `border-focus` with offset 2. |
| Unread / thread-activity dot (sidebar) | 6×6 circle, `affordance-prominent` (`#000` / `#fff`) |
| Unread label | `text-navigation-unread` (`#262626` / `#fff`) at weight 500 |
| Availability capsule | Button recipe with the `interaction-availability` fills (1.4). Profile variant (`interaction-profile-availability`, online only): min-height 20, padding 2 8. |
| Count/notification badges | No shared Badge component exists in the DS. Feature badges compose the roles above. |

## 7. Icons

### 7.1 Gateway and defaults

- All UI glyphs go through one gateway, `src/shared/design-system/icons/index.ts`, which is the only importer of `@tabler/icons-react`. `grep "@tabler" src` outside `icons/` returns nothing. Each icon is imported per file from `@tabler/icons-react/dist/esm/icons/Icon*.mjs` and wrapped by `defineIcon("tabler"|"custom", …)`.
- `src/shared/design-system/icons/inventory.ts` enumerates the exports into `TABLER_ICONS` and `CUSTOM_ICONS` for the design-system gallery. An export without a classification throws.
- Defaults are set in `src/shared/design-system/icons/createDecorativeIcon.tsx:38-50`:

| Property | Value |
|---|---|
| Default size | `1em`, which follows the parent font size |
| Numeric `size` prop | converted to rem: `size/16 rem`. `size={16}` becomes `1rem` and scales with the text-scale setting |
| `aria-hidden` | `true` by default |
| Stroke width | Tabler default `2` on a 24x24 viewBox, `stroke-linecap/linejoin: round`, `fill:none`, `stroke: currentColor`. Overrides: `strokeWidth={2.5}` in sidebar section headers (`src/bundled/channels/SidebarSection.tsx`, sizes 15), `src/app/shell/PageNavigation.tsx:40`, and session sections; `1.5` in `src/features/communities/CommunityRail.tsx:199` (globe 22) |
| Color | `currentColor` |
| Button override | Inside `.buzz-button-label`, CSS forces SVG children to 24px (md), 16px (sm/compact/xs), or 12px (`data-icon-size="xs"`) (`components.css:44-74,197-200`). Empty-state icons are 32px (`empty-state.css:23-26`) |

Most common call-site sizes, counted over all `Icon … size={n}` occurrences: 16 (127), 14 (79), 18 (34), 20 (27), 17 (16), 15 (12), 24 (8), 32 (7), 22 (5), 12 (4), 48 (3).

### 7.2 Tabler icons (all 133 gateway exports)

Columns:

- **SVG** is the path under `node_modules/@tabler/icons/icons/`.
- **Where used** lists files grouped by scope:
  - shell = `src/app/shell`
  - sidebar = `src/features/channel-navigation`
  - channels = `src/bundled/channels`
  - messages = `src/features/messages`
  - agents = `src/bundled/agents`
  - settings = `src/app/*Settings*` and `src/app/settings-sections.ts`
  - other = anything else (non-test files)

Design-system components that use icons internally also matter:

- `Select`, `Combobox`, `Accordion`, `FieldButton`: `chevron-down`.
- `Menu`: `chevron-right` and `check`.
- `Checkbox`: `check` and `minus`.
- `Dialog`, `Toast`, `Tabs`, `SearchField`: `x`.
- `Button`: `loader-2`.

Two more indirect uses: channel rows get `hash` / `lock` via `src/features/channels/channel-icon.ts`, and shell nav icons come from `src/app/shell/presentation.ts`.

| Export | SVG | Style | Where used |
|---|---|---|---|
| `AlarmIcon` | `outline/alarm.svg` | outline | shell (presentation.ts); other: bundled/reminders/RemindersPage.tsx, bundled/reminders/index.tsx |
| `ArchiveIcon` | `outline/archive.svg` | outline | channels (ChannelLifecycleActions.tsx, ChannelLifecycleMenu.tsx); agents (AgentCard.tsx); other: bundled/hosted-communities/HostedCommunities.tsx |
| `ArchiveOffIcon` | `outline/archive-off.svg` | outline | agents (AgentCard.tsx) |
| `ArrowClockwiseIcon` | `outline/rotate-clockwise.svg` | outline | channels (ChannelLifecycleMenu.tsx); messages (VideoPlayer.tsx); other: features/sessions/AgentChoice.tsx |
| `ArrowCounterClockwiseIcon` | `outline/rotate.svg` | outline | messages (VideoPlayer.tsx); settings (AppearanceSettings.tsx) |
| `ArrowDownIcon` | `outline/arrow-down.svg` | outline | channels (SidebarUnread.tsx); messages (JumpToLatestButton.tsx); other: bundled/workflows/WorkflowForm.tsx |
| `ArrowLeftIcon` | `outline/arrow-left.svg` | outline | shell (NavigationControls.tsx); settings (SettingsSidebar.tsx); other: bundled/agents2/Agents2Page.tsx, bundled/workflows/WorkflowsPage.tsx, features/panels/PanelSubview.tsx |
| `ArrowRightIcon` | `outline/arrow-right.svg` | outline | shell (NavigationControls.tsx); other: bundled/workflows/WorkflowLanding.tsx |
| `ArrowSquareLeftIcon` | `outline/square-arrow-left.svg` | outline | unused (gallery only) |
| `ArrowSquareOutIcon` | `outline/external-link.svg` | outline | messages (ImageReviewStage.tsx); settings (AgentSettings.tsx); other: bundled/github/index.tsx, bundled/hosted-communities/HostedCommunities.tsx, bundled/inbox/DraftsView.tsx, bundled/inbox/InboxDetail.tsx |
| `ArrowSquareRightIcon` | `outline/square-arrow-right.svg` | outline | unused (gallery only) |
| `ArrowUpIcon` | `outline/arrow-up.svg` | outline | channels (SidebarUnread.tsx); messages (MessageComposer.tsx); other: shared/design-system/ui/Composer.tsx |
| `ArrowUpRightIcon` | `outline/arrow-up-right.svg` | outline | other: features/communities/CommunityDialog.tsx |
| `ArrowsClockwiseIcon` | `outline/refresh.svg` | outline | channels (ChannelMembersDialog.tsx); messages (ComposerAttachments.tsx); agents (AgentCard.tsx, AgentLibrary.tsx); settings (AgentSettings.tsx); other: bundled/channel-templates/TemplateLibrary.tsx, bundled/hosted-communities/HostedCommunities.tsx, bundled/terminal/TerminalPanel.tsx, bundled/todos/TodosPanel.tsx, bundled/workflows/WorkflowsPage.tsx, features/browser/BrowserHostView.tsx, features/updates/UpdateSettings.tsx |
| `ArrowsDownUpIcon` | `outline/arrows-down-up.svg` | outline | channels (SidebarSection.tsx) |
| `ArrowsInIcon` | `outline/arrows-minimize.svg` | outline | other: shared/design-system/ui/FlexWorkspace.tsx |
| `ArrowsLeftRightIcon` | `outline/arrows-left-right.svg` | outline | other: bundled/hosted-communities/HostedCommunities.tsx |
| `ArrowsOutIcon` | `outline/arrows-maximize.svg` | outline | shell (WindowControls.tsx); messages (MediaAttachment.tsx); other: bundled/diffs/DiffMessage.tsx, shared/design-system/ui/FlexWorkspace.tsx |
| `AtIcon` | `outline/at.svg` | outline | messages (MessageMarkdown.tsx, ReferenceText.tsx); other: bundled/mentions/MentionPicker.tsx, features/agents2/AttentionPanel.tsx |
| `BellIcon` | `outline/bell.svg` | outline | shell (presentation.ts); sidebar (ChannelSidebar.tsx); messages (MessageManagement.tsx); settings (settings-sections.ts); other: bundled/inbox/InboxPage.tsx |
| `BellSlashIcon` | `outline/bell-off.svg` | outline | sidebar (ChannelSidebar.tsx); messages (MessageManagement.tsx) |
| `BoxArrowUpIcon` | `outline/upload.svg` | outline | other: bundled/hosted-communities/HostedCommunities.tsx |
| `BrowserIcon` | `outline/browser.svg` | outline | shell (PageIcon.tsx, presentation.ts); channels (useConversationTabs.tsx); other: features/conversation/MessageLink.tsx, features/panels/PanelWorkspace.tsx |
| `BulbIcon` | `outline/bulb.svg` | outline | other: bundled/agent-activity/ActivityTranscript.tsx |
| `CalendarIcon` | `outline/calendar.svg` | outline | shell (SearchResults.tsx); other: bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx |
| `CaretDownIcon` | `outline/chevron-down.svg` | outline | channels (ChannelSidebarRow.tsx, SidebarSection.tsx); agents (CommunityCatalog.tsx, InventoryIdentityCard.tsx); other: bundled/github/GitHubConversation.tsx, bundled/inbox/InboxFilters.tsx, bundled/moderation/CommunityAdmin.tsx, bundled/sessions/SessionSections.tsx, bundled/terminal/TerminalPanel.tsx, shared/design-system/ui/Accordion.tsx, shared/design-system/ui/Combobox.tsx, shared/design-system/ui/FieldButton.tsx, shared/design-system/ui/FlexWorkspace.tsx, shared/design-system/ui/Select.tsx |
| `CaretLeftIcon` | `outline/chevron-left.svg` | outline | messages (ImageReviewStage.tsx); other: features/browser/BrowserHostView.tsx, shared/design-system/ui/Calendar.tsx |
| `CaretRightIcon` | `outline/chevron-right.svg` | outline | channels (ChannelSettingsPanel.tsx, ChannelSidebarRow.tsx); messages (ImageReviewStage.tsx); other: bundled/profiles/ProfileAddChannel.tsx, bundled/profiles/ProfileRuntime.tsx, features/browser/BrowserHostView.tsx, shared/design-system/ui/Calendar.tsx, shared/design-system/ui/Menu.tsx |
| `CaretUpIcon` | `outline/chevron-up.svg` | outline | channels (SidebarSection.tsx); other: bundled/github/GitHubConversation.tsx, features/sessions/AgentChoice.tsx |
| `ChatCircleIcon` | `outline/message-circle.svg` | outline | shell (SearchChoices.tsx, SearchResults.tsx); sidebar (ChannelSidebar.tsx); channels (ChannelSidebarItem.tsx, ChannelsPage.tsx); messages (MediaReviewViewer.tsx, MessageActionBar.tsx); settings (SettingsSidebar.tsx, settings-sections.ts); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/feedback/index.tsx, bundled/github/GitHubConversation.tsx, bundled/inbox/DraftsView.tsx, bundled/inbox/InboxDetail.tsx, bundled/links/InlineLink.tsx, bundled/sessions/SessionsPage.tsx, bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx, features/settings/service.ts, shared/design-system/ui/InlineChip.tsx |
| `ChatsCircleIcon` | `outline/messages.svg` | outline | shell (presentation.ts); other: bundled/links/InlineLink.tsx |
| `CheckCircleIcon` | `outline/circle-check.svg` | outline | agents (AgentModelPicker.tsx); other: bundled/github/GitHubChecks.tsx, bundled/github/GitHubConversation.tsx, bundled/hosted-communities/HostedCommunities.tsx, bundled/todos/TodosPanel.tsx |
| `CheckIcon` | `outline/check.svg` | outline | shell (ProfileButton.tsx); other: bundled/github/GitHubChecks.tsx, bundled/pairing/PairingSettings.tsx, bundled/profiles/ProfilePublicMetadata.tsx, shared/design-system/ui/Checkbox.tsx, shared/design-system/ui/Combobox.tsx, shared/design-system/ui/Menu.tsx, shared/design-system/ui/Select.tsx |
| `ChecksIcon` | `outline/checks.svg` | outline | other: features/communities/CommunityRailItem.tsx |
| `CircleDashedIcon` | `outline/circle-dashed.svg` | outline | other: bundled/github/GitHubChecks.tsx, bundled/github/index.tsx |
| `CircleHalfIcon` | `outline/circle-half.svg` | outline | other: bundled/todos/TodosPanel.tsx |
| `CircleIcon` | `outline/circle.svg` | outline | other: bundled/todos/TodosPanel.tsx |
| `CircleNotchIcon` | `outline/loader-2.svg` | outline | shell (ProfileButton.tsx); channels (ChannelMembersDialog.tsx, ChannelSettingsPanel.tsx); messages (ComposerAttachments.tsx); agents (AgentModelPicker.tsx); other: bundled/hosted-communities/HostedCommunities.tsx, bundled/pairing/PairingSettings.tsx, bundled/voice-notes/VoiceNoteCard.tsx, shared/design-system/ui/Button.tsx, shared/design-system/ui/Combobox.tsx |
| `CloudUploadIcon` | `outline/cloud-upload.svg` | outline | other: features/profiles/AvatarEditor.tsx |
| `CodeBlockIcon` | `outline/source-code.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `CodeIcon` | `outline/code.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `ColumnsIcon` | `outline/columns.svg` | outline | unused (gallery only) |
| `CopyIcon` | `outline/copy.svg` | outline | channels (ChannelSettingsPanel.tsx); messages (ImageReviewStage.tsx, MessageActionBar.tsx); agents (AgentCard.tsx); settings (AgentSettings.tsx, ProfileSettings.tsx); other: bundled/agents2/Agents2Page.tsx, bundled/channel-templates/index.tsx, bundled/profiles/ProfileHarnessLog.tsx, bundled/profiles/ProfilePanel.tsx, bundled/profiles/ProfilePublicMetadata.tsx, bundled/projects/ProjectsPage.tsx, bundled/sessions/SessionSections.tsx, features/conversation/MessageLink.tsx, shared/identity/IdentityRow.tsx |
| `CrownIcon` | `outline/crown.svg` | outline | other: bundled/moderation/CommunityAdmin.tsx |
| `DetectiveIcon` | `outline/spy.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `DotsThreeIcon` | `outline/dots.svg` | outline | shell (AppShell.tsx); channels (ChannelHeaderMenu.tsx, MemberAdministration.tsx, SidebarSection.tsx); messages (MessageActionBar.tsx); agents (AgentCard.tsx, UnifiedInventory.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/channel-templates/TemplateLibrary.tsx, bundled/github/GitHubChecks.tsx, bundled/github/GitHubConversation.tsx, bundled/moderation/CommunityAdmin.tsx, bundled/sessions/SessionSections.tsx, bundled/sessions/SessionsPage.tsx, bundled/workflows/WorkflowEditor.tsx, bundled/workflows/WorkflowLanding.tsx, features/agents2/AttentionPanel.tsx, features/browser/BrowserHostView.tsx, features/sessions/NewSessionComposer.tsx |
| `DotsThreeVerticalIcon` | `outline/dots-vertical.svg` | outline | unused (gallery only) |
| `DownloadIcon` | `outline/download.svg` | outline | messages (FileAttachment.tsx, ImageReviewStage.tsx); settings (settings-sections.ts) |
| `DropboxLogoIcon` | `outline/brand-dropbox.svg` | outline | other: bundled/links/InlineLink.tsx |
| `EnvelopeIcon` | `outline/mail.svg` | outline | channels (ChannelReadMenuItem.tsx); messages (MessageManagement.tsx) |
| `EnvelopeOpenIcon` | `outline/mail-opened.svg` | outline | channels (ChannelActivityPopover.tsx, ChannelReadMenuItem.tsx); messages (MessageManagement.tsx) |
| `EyeIcon` | `outline/eye.svg` | outline | agents (ProviderApiKeyField.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/workflows/WorkflowLanding.tsx, features/agents2/AttentionPanel.tsx |
| `EyeSlashIcon` | `outline/eye-off.svg` | outline | channels (ChannelLifecycleMenu.tsx); agents (ProviderApiKeyField.tsx); other: bundled/workflows/WorkflowChannel.tsx |
| `FigmaLogoIcon` | `outline/brand-figma.svg` | outline | other: bundled/links/InlineLink.tsx |
| `FileTextIcon` | `outline/file-text.svg` | outline | channels (ChannelHeaderMenu.tsx); messages (ComposerAttachments.tsx, FileAttachment.tsx); other: bundled/channel-templates/TemplateLibrary.tsx, bundled/links/InlineLink.tsx, bundled/profiles/ProfileRuntime.tsx, bundled/projects/ProjectsPage.tsx, features/conversation/MessageLink.tsx |
| `FlagIcon` | `outline/flag.svg` | outline | messages (MessageRow.tsx, ReportMessageDialog.tsx) |
| `FolderOpenIcon` | `outline/folder-open.svg` | outline | other: app/PluginImport.tsx |
| `FolderSimpleIcon` | `outline/folder.svg` | outline | shell (presentation.ts); sidebar (ChannelSidebar.tsx); channels (ChannelSetupSettings.tsx, index.tsx); other: bundled/github/index.tsx, bundled/sessions/SessionSections.tsx |
| `GearIcon` | `outline/settings.svg` | outline | shell (ProfileButton.tsx, presentation.ts); channels (ChannelSettingsPanel.tsx, ChannelsPage.tsx); other: bundled/sessions/SessionSections.tsx, bundled/sessions/SessionsPage.tsx, features/communities/CommunityRailItem.tsx, features/sessions/NewSessionComposer.tsx |
| `GitBranchIcon` | `outline/git-branch.svg` | outline | other: app/PluginImport.tsx, bundled/projects/ProjectsPage.tsx |
| `GitCommitIcon` | `outline/git-commit.svg` | outline | other: bundled/github/index.tsx |
| `GitMergeIcon` | `outline/git-merge.svg` | outline | other: bundled/github/GitHubConversation.tsx, bundled/github/index.tsx |
| `GitPullRequestIcon` | `outline/git-pull-request.svg` | outline | other: bundled/github/index.tsx, bundled/projects/ResourcePicker.tsx, bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx |
| `GithubLogoIcon` | `outline/brand-github.svg` | outline | other: bundled/links/InlineLink.tsx |
| `GitlabLogoIcon` | `outline/brand-gitlab.svg` | outline | other: bundled/links/InlineLink.tsx |
| `GlobeIcon` | `outline/world.svg` | outline | other: bundled/agent-activity/ActivityTranscript.tsx, bundled/hosted-communities/HostedCommunities.tsx, bundled/hosted-communities/index.tsx, bundled/links/InlineLink.tsx, features/communities/CommunityRail.tsx |
| `GoogleDriveLogoIcon` | `outline/brand-google-drive.svg` | outline | other: bundled/links/InlineLink.tsx |
| `HashIcon` | `outline/hash.svg` | outline | other: bundled/inbox/DraftsView.tsx, bundled/inbox/InboxDetail.tsx, bundled/links/InlineLink.tsx, bundled/workflows/WorkflowChannel.tsx, bundled/workflows/WorkflowChannelPicker.tsx, features/channels/channel-icon.ts |
| `HouseIcon` | `outline/home.svg` | outline | unused (gallery only) |
| `InfoIcon` | `outline/info-circle.svg` | outline | channels (ChannelHeaderMenu.tsx, ChannelTabPicker.tsx); messages (WorkflowByline.tsx); other: bundled/profiles/ProfileAgentArchive.tsx, bundled/todos/TodosPanel.tsx |
| `KeyboardIcon` | `outline/keyboard.svg` | outline | settings (settings-sections.ts) |
| `LightningIcon` | `outline/bolt.svg` | outline | shell (presentation.ts); channels (ChannelActivityPopover.tsx); messages (MessageRow.tsx); other: bundled/inbox/InboxPage.tsx, bundled/workflows/WorkflowEditor.tsx, bundled/workflows/WorkflowLanding.tsx, features/conversation/BuzzLinkPreview.tsx |
| `LinkBreakIcon` | `outline/unlink.svg` | outline | other: bundled/hosted-communities/HostedCommunities.tsx |
| `LinkIcon` | `outline/link.svg` | outline | channels (ChannelCopyLinkMenuItem.tsx); messages (ComposerFormattingTools.tsx, MessageActionBar.tsx); other: bundled/github/GitHubChecks.tsx, bundled/github/GitHubConversation.tsx, features/communities/CommunityRailItem.tsx, features/profiles/AvatarEditor.tsx, shared/design-system/ui/InlineChip.tsx |
| `ListBulletsIcon` | `outline/list.svg` | outline | messages (ComposerFormattingTools.tsx); other: bundled/workflows/WorkflowChannel.tsx, features/agents/AgentActivityRow.tsx |
| `ListChecksIcon` | `outline/list-check.svg` | outline | channels (ChannelTabPicker.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/todos/TodosPanel.tsx, bundled/todos/index.tsx |
| `ListNumbersIcon` | `outline/list-numbers.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `LockIcon` | `outline/lock.svg` | outline | other: bundled/links/InlineLink.tsx, bundled/workflows/WorkflowChannelPicker.tsx, features/channels/channel-icon.ts, features/conversation/BuzzLinkPreview.tsx |
| `MagnifyingGlassIcon` | `outline/search.svg` | outline | shell (PageSearch.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, shared/design-system/ui/SearchField.tsx |
| `MicrophoneIcon` | `outline/microphone.svg` | outline | other: bundled/voice-notes/index.tsx |
| `MicrosoftTeamsLogoIcon` | `outline/brand-teams.svg` | outline | other: bundled/links/InlineLink.tsx |
| `MinusIcon` | `outline/minus.svg` | outline | shell (WindowControls.tsx); sidebar (ChannelSidebar.tsx); channels (ChannelDurationField.tsx); messages (ImageReviewStage.tsx); other: bundled/moderation/CommunityAdmin.tsx, shared/design-system/ui/Checkbox.tsx |
| `MonitorIcon` | `outline/device-desktop.svg` | outline | unused (gallery only) |
| `MoonIcon` | `outline/moon.svg` | outline | unused (gallery only) |
| `NotificationIcon` | `outline/notification.svg` | outline | other: bundled/inbox/InboxFilters.tsx |
| `NotionLogoIcon` | `outline/brand-notion.svg` | outline | other: bundled/links/InlineLink.tsx |
| `PaletteIcon` | `outline/palette.svg` | outline | settings (settings-sections.ts) |
| `PaperclipIcon` | `outline/paperclip.svg` | outline | messages (MessageComposer.tsx) |
| `PauseFilledIcon` | `filled/player-pause.svg` | filled | messages (AudioAttachment.tsx, VideoPlayer.tsx) |
| `PauseIcon` | `outline/player-pause.svg` | outline | other: bundled/voice-notes/VoiceNoteCard.tsx |
| `PencilSimpleIcon` | `outline/pencil.svg` | outline | channels (ChannelSettingsPanel.tsx); messages (MessageComposer.tsx, MessageManagement.tsx); agents (AgentCard.tsx); settings (ShortcutSettings.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/sessions/SessionSections.tsx, bundled/sessions/SessionsPage.tsx, bundled/workflows/WorkflowEditor.tsx, bundled/workflows/WorkflowLanding.tsx, features/agents2/AttentionPanel.tsx, features/profiles/AvatarEditor.tsx |
| `PlayFilledIcon` | `filled/player-play.svg` | filled | messages (AudioAttachment.tsx, VideoPlayer.tsx) |
| `PlayIcon` | `outline/player-play.svg` | outline | other: bundled/voice-notes/VoiceNoteCard.tsx, bundled/workflows/WorkflowChannel.tsx, bundled/workflows/WorkflowLanding.tsx |
| `PlugIcon` | `outline/plug.svg` | outline | shell (ProfileButton.tsx); channels (ChannelsPage.tsx) |
| `PlusIcon` | `outline/plus.svg` | outline | sidebar (ChannelSidebar.tsx); channels (ChannelDurationField.tsx, SidebarSection.tsx); messages (ImageReviewStage.tsx); agents (AgentControlPanel.tsx, AgentCreateDialog.tsx); settings (AgentSettings.tsx); other: bundled/agents2/Agents2Page.tsx, bundled/channel-templates/ChannelTemplatesDialog.tsx, bundled/channel-templates/TemplateLibrary.tsx, bundled/sessions/SessionSections.tsx, bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx, features/agents2/AttentionPanel.tsx, features/communities/CommunityRail.tsx, features/panels/PanelWorkspace.tsx, features/profiles/AvatarEditor.tsx |
| `PresentationIcon` | `outline/presentation.svg` | outline | other: bundled/links/InlineLink.tsx |
| `QuestionIcon` | `outline/help-circle.svg` | outline | settings (AgentSettings.tsx) |
| `QuotesIcon` | `outline/blockquote.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `RobotIcon` | `custom/robot.svg` (Soft & wide mark, `icons/Robot.tsx`) | outline | shell (presentation.ts); messages (MessageMarkdown.tsx, ReferenceText.tsx); settings (AgentSettings.tsx, settings-sections.ts); other: features/sessions/AgentChoice.tsx |
| `ShieldIcon` | `outline/shield.svg` | outline | other: bundled/agent-activity/ActivityTranscript.tsx, bundled/moderation/CommunityAdmin.tsx |
| `SidebarIcon` | `outline/layout-sidebar.svg` | outline | shell (AppShell.tsx, PanelLaunchers.tsx); messages (MediaReviewViewer.tsx) |
| `SidebarRightIcon` | `outline/layout-sidebar-right.svg` | outline | channels (ChannelsPage.tsx); other: bundled/sessions/SessionsPage.tsx |
| `SignOutIcon` | `outline/logout.svg` | outline | channels (ChannelLifecycleActions.tsx, ChannelLifecycleMenu.tsx); other: bundled/hosted-communities/HostedCommunities.tsx, features/communities/CommunityRailItem.tsx |
| `SlackLogoIcon` | `outline/brand-slack.svg` | outline | other: bundled/links/InlineLink.tsx |
| `SlidersHorizontalIcon` | `outline/adjustments-horizontal.svg` | outline | unused (gallery only) |
| `SmileyIcon` | `outline/mood-smile.svg` | outline | shell (ProfileButton.tsx); other: bundled/emoji/CustomEmojiSettings.tsx, bundled/emoji/EmojiPicker.tsx, bundled/emoji/index.tsx, bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx, features/user-status/StatusEditor.tsx |
| `SmileyPlusIcon` | `outline/mood-plus.svg` | outline | other: bundled/emoji/EmojiPicker.tsx, bundled/emoji/ReactionPicker.tsx |
| `SmileyStickerIcon` | `outline/sticker-2.svg` | outline | unused (gallery only) |
| `SpeakerHighIcon` | `outline/volume.svg` | outline | messages (VideoPlayer.tsx) |
| `SpeakerSlashIcon` | `outline/volume-off.svg` | outline | messages (VideoPlayer.tsx) |
| `SquaresFourIcon` | `outline/layout-grid.svg` | outline | settings (Settings.tsx, settings-sections.ts) |
| `StopIcon` | `outline/player-stop.svg` | outline | other: bundled/terminal/TerminalPanel.tsx, bundled/voice-notes/VoiceCapture.tsx |
| `SunIcon` | `outline/sun.svg` | outline | unused (gallery only) |
| `TableIcon` | `outline/table.svg` | outline | other: bundled/links/InlineLink.tsx |
| `TerminalWindowIcon` | `outline/terminal-2.svg` | outline | channels (ChannelTabPicker.tsx); settings (AgentSettings.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/terminal/TerminalPanel.tsx, bundled/terminal/index.tsx, features/panels/PanelWorkspace.tsx |
| `TextAaIcon` | `outline/letter-case.svg` | outline | channels (SidebarSection.tsx); messages (ComposerFormattingTools.tsx) |
| `TextBIcon` | `outline/bold.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `TextItalicIcon` | `outline/italic.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `TextStrikethroughIcon` | `outline/strikethrough.svg` | outline | messages (ComposerFormattingTools.tsx) |
| `ThumbsUpIcon` | `outline/thumb-up.svg` | outline | unused (gallery only) |
| `TicketIcon` | `outline/ticket.svg` | outline | other: features/communities/CommunityRailItem.tsx |
| `TimerIcon` | `outline/stopwatch.svg` | outline | channels (SidebarSection.tsx); other: bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx, features/agents2/AttentionPanel.tsx |
| `TrashIcon` | `outline/trash.svg` | outline | channels (ChannelLifecycleActions.tsx, ChannelLifecycleMenu.tsx, SidebarSection.tsx); messages (MessageManagement.tsx); agents (AgentCard.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/hosted-communities/HostedCommunities.tsx, bundled/inbox/DraftsView.tsx, bundled/workflows/WorkflowChannel.tsx, bundled/workflows/WorkflowConditions.tsx, bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx, bundled/workflows/WorkflowWebhookFields.tsx, features/agents2/AgentManage.tsx, features/agents2/AttentionPanel.tsx |
| `UserIcon` | `outline/user.svg` | outline | shell (ProfileButton.tsx); settings (settings-sections.ts); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/moderation/CommunityAdmin.tsx, bundled/todos/TodosPanel.tsx |
| `UsersIcon` | `outline/users.svg` | outline | channels (ChannelMembersDialog.tsx, ChannelsPage.tsx); settings (settings-sections.ts); other: bundled/channel-templates/TemplateLibrary.tsx, bundled/sessions/SessionShare.tsx |
| `VideoCameraIcon` | `outline/video.svg` | outline | other: bundled/links/InlineLink.tsx |
| `VideoConferenceIcon` | `outline/video-plus.svg` | outline | other: bundled/links/InlineLink.tsx |
| `WarningCircleIcon` | `outline/alert-circle.svg` | outline | channels (ChannelLifecycleMenu.tsx); messages (ComposerAttachments.tsx); agents (AgentModelPicker.tsx); other: bundled/agent-activity/ActivityTranscript.tsx, bundled/github/GitHubChecks.tsx, bundled/hosted-communities/HostedCommunities.tsx, bundled/voice-notes/VoiceNoteCard.tsx, features/agents2/AttentionPanel.tsx |
| `WebhooksLogoIcon` | `outline/webhook.svg` | outline | other: bundled/workflows/WorkflowForm.tsx, bundled/workflows/WorkflowLanding.tsx |
| `WrenchIcon` | `outline/tool.svg` | outline | settings (settings-sections.ts); other: bundled/agent-activity/ActivityTranscript.tsx |
| `XCircleIcon` | `outline/circle-x.svg` | outline | other: bundled/github/GitHubChecks.tsx, bundled/github/GitHubConversation.tsx, bundled/github/index.tsx |
| `XIcon` | `outline/x.svg` | outline | shell (WindowControls.tsx); channels (ChannelSidebarRow.tsx); messages (ComposerAttachments.tsx, ComposerFormattingTools.tsx, MediaAttachment.tsx, MediaReviewViewer.tsx, MessageComposer.tsx, ThreadPanel.tsx); agents (AgentEditor.tsx); settings (ShortcutSettings.tsx); other: bundled/agents2/Agents2Page.tsx, bundled/github/GitHubConversation.tsx, bundled/inbox/DraftsView.tsx, bundled/inbox/InboxDetail.tsx, bundled/todos/TodosPanel.tsx, bundled/voice-notes/VoiceNoteCard.tsx, bundled/voice-notes/VoiceNoteRecorder.tsx, bundled/workflows/WorkflowForm.tsx, features/direct-messages/RecipientPicker.tsx, features/panels/PanelCard.tsx, features/panels/PanelSubview.tsx, shared/design-system/ui/Dialog.tsx, shared/design-system/ui/FlexWorkspace.tsx, shared/design-system/ui/SearchField.tsx, shared/design-system/ui/Tabs.tsx, shared/design-system/ui/Toast.tsx |
| `YoutubeLogoIcon` | `outline/brand-youtube.svg` | outline | other: bundled/links/InlineLink.tsx |

Exports with no non-test use, kept for the gallery:

- `ArrowSquareLeftIcon`, `ArrowSquareRightIcon`, `DotsThreeVerticalIcon`
- `HouseIcon`, `MoonIcon`, `MonitorIcon`, `SunIcon`
- `SmileyStickerIcon`, `SlidersHorizontalIcon`
- `ThumbsUpIcon`, `ColumnsIcon`

Raw SVG strings for the emoji picker are imported with `?raw` from `@tabler/icons/outline/*.svg` (`src/shared/design-system/icons/svg.ts`) and used in `src/bundled/emoji/emoji-mart.ts:16-28`:

| Category | SVG |
|---|---|
| frequent | `clock.svg` |
| people | `mood-smile.svg` |
| nature | `paw.svg` |
| foods | `lemon-2.svg` |
| activity | `barbell.svg` |
| places | `car.svg` |
| objects | `bulb.svg` |
| symbols | `category.svg` |
| flags | `flag.svg` |
| custom / buzz-custom | `asterisk.svg` |
| search | `search.svg` |
| clear | `x.svg` |

The picker CSS sets `svg.icons-tabler-outline { fill: none }`.

### 7.3 Custom (non-Tabler) icons

All of these live in `src/shared/design-system/icons/`.

| Export | Source file | Artwork | Intended size | Used in |
|---|---|---|---|---|
| `HashArrowInIcon` | `HashArrowIn.tsx` | 24x24 stroke 2 round, `currentColor`; path `M4 9h16M4 15h7M10 3 8 21M16 3l-1 9M21 18h-7m3-3-3 3 3 3` ("Send to channel") | 16 | `src/features/messages/MessageActionBar.tsx` |
| `NotificationFilledIcon` | `NotificationFilled.tsx` | 24x24 fill `currentColor`; path `M10 5H7a3 3 0 0 0-3 3v9a3 3 0 0 0 3 3h9a3 3 0 0 0 3-3v-3a1 1 0 0 0-1-1h-1a6 6 0 0 1-6-6V6a1 1 0 0 0-1-1Z` + circle (17,7) r4 | 20 | `src/bundled/inbox/InboxFilters.tsx` |
| `BestieIcon` | `BestieMark.tsx` | 256x256 SVG `<image>` of `public/bestie.png` | 15, 17 | `src/app/shell/presentation.ts`, `src/bundled/bestie/index.tsx` |
| `GitHubIssueIcon` | `GitHubIssue.tsx` | 256x256 fill; ring (r 104 / 88) + center dot r 32 | 22 | `src/bundled/github/index.tsx` |
| `OneDriveLogoIcon` | `OneDriveLogo.tsx` | 256x256 outline cloud, `currentColor` | 14 | `src/bundled/links/InlineLink.tsx` |
| `GooseLogoIcon` | `HarnessLogos.tsx` | 45x45 fill `currentColor` (Block Goose mark, Apache-2.0) | 32 | `src/app/AgentSettings.tsx` |
| `PiLogoIcon` | `HarnessLogos.tsx` | 800x800: rect rx120 `#09090b` + white pi glyph paths (MIT, Earendil) | 32 | `src/app/AgentSettings.tsx` |
| `ClaudeLogoIcon` | `HarnessLogos.tsx` + `claude.png` (11,536 B) | 32x32 `<image>` (inlined PNG) | 32 | `src/app/ClaudeHarnessSetup.tsx` |
| `HermesLogoIcon` | `HarnessLogos.tsx` + `hermes.png` (1,698 B) | 32x32 `<image>` (inlined PNG) | 24, 32, 48 | `src/app/AgentSettings.tsx` |

Brand-mark attribution is in `NOTICE.md` ("Harness brand marks"). Other app artwork lives in `public/`: `buzz-mark.svg`, `buzz-loading-mark.svg`, `agent-activity.svg`, `app-icon.png`, `bestie.png`. Avatar mask SVGs are in `src/shared/design-system/styles/`: `avatar-squircle.svg`, `avatar-status-cutout.svg`, `avatar-status-squircle-cutout.svg`.

### 7.4 Packages, file locations and license

| Item | Value |
|---|---|
| Packages | `@tabler/icons` 3.46.0 and `@tabler/icons-react` 3.46.0, both pinned exactly (`package.json:50-51`) |
| Installed at | `node_modules/@tabler/icons`, a symlink to `node_modules/.pnpm/@tabler+icons@3.46.0/node_modules/@tabler/icons` |
| Outline SVGs | `node_modules/@tabler/icons/icons/outline/<name>.svg` (5,130 files) |
| Filled SVGs | `node_modules/@tabler/icons/icons/filled/<name>.svg` (1,054 files) |
| Also shipped | `icons.json`, `tabler-nodes-outline.json`, `tabler-nodes-filled.json` (path nodes per icon, handy for codegen) |
| React package | `node_modules/.pnpm/@tabler+icons-react@3.46.0_react@19.2.8/node_modules/@tabler/icons-react` |
| SVG template | `<svg xmlns=… width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">` + `<path stroke="none" d="M0 0h24v24H0z" fill="none"/>` (bounding-box path; skip it) |
| License | MIT. `node_modules/@tabler/icons/LICENSE`: "MIT License / Copyright (c) 2020-2026 Paweł Kuna" |

## 8. Fonts

### 8.1 Imports

All font imports are in `src/main.tsx:8-10`. Nothing else in `src/` or `index.html` loads fonts.

```ts
import "@fontsource-variable/inter/wght.css";
import "@fontsource-variable/inter/wght-italic.css";
import "@fontsource/jetbrains-mono/400.css";
```

| Family (CSS name) | Package / version | Files loaded | Axes / weights | Styles | Subsets (each its own `@font-face` with `unicode-range`) |
|---|---|---|---|---|---|
| `Inter Variable` | `@fontsource-variable/inter` 5.3.0 (`package.json:45` `^5.2.8`; Google Fonts build "v20", 2025-09-10) | `inter-{subset}-wght-normal.woff2`, `inter-{subset}-wght-italic.woff2` (`format('woff2-variations')`, `font-display: swap`, `font-weight: 100 900`) | `wght` 100–900 only. The `opsz` axis (14–32) exists in the package (`opsz.css`) but is **not** imported, so optical size stays at its default | normal + italic | cyrillic-ext, cyrillic, greek-ext, greek, vietnamese, latin-ext, latin (7 x 2 = 14 files) |
| `JetBrains Mono` | `@fontsource/jetbrains-mono` 5.3.0 (`package.json:46`) | `jetbrains-mono-{subset}-400-normal.woff2` (+ `.woff` fallback), static | 400 only | normal only | cyrillic-ext, cyrillic, greek, vietnamese, latin-ext, latin (6 files) |

Unicode ranges:

- latin: `U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,U+0304,U+0308,U+0329,U+2000-206F,U+20AC,U+2122,U+2191,U+2193,U+2212,U+2215,U+FEFF,U+FFFD`.
- Other subsets: see `node_modules/@fontsource-variable/inter/wght.css` and `node_modules/@fontsource/jetbrains-mono/400.css`.

### 8.2 Family stacks and rendering settings

Sources: `src/shared/design-system/styles/typography.css:4-41`, `typography-base.css:5-31`, `src/shared/styles/globals.css:56,409-458`.

| Token | Value |
|---|---|
| `--font-sans` | `"Inter Variable", Inter, ui-sans-serif, system-ui, -apple-system, sans-serif` |
| `--font-system-mono` / `--font-mono` / `--buzz-mono-family` | `"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace` |
| `--font-emoji` | `"Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", sans-serif`. Used only in `src/features/messages/VideoPlayer.module.css:377` |
| Weights in use | `--type-weight-normal: 400`, `--type-weight-medium: 500`. `semibold` is aliased to 500 (`--font-weight-semibold: 500`), **except** inside the channel sidebar, where `src/bundled/channels/Channels.module.css:43` redefines `--type-weight-semibold: 600` (unread/selected rows render at 600, see 3.1). No other 600/700 in app CSS; UA `<strong>`/`<b>` render as `bold` (700) on the variable axis |
| Smoothing | `-webkit-font-smoothing: antialiased; -moz-osx-font-smoothing: grayscale` on `html` |
| Other | `font-synthesis-weight: none`, so JetBrains Mono bold is **not** faked and stays 400. `text-rendering: optimizeLegibility` |
| `font-feature-settings` / `font-variation-settings` | **None anywhere in `src/`** (no `cv11`, `ss01`, etc.). Inter's default features apply as-is |
| Numerals | `font-variant-numeric: tabular-nums` on `code, kbd, pre, samp`, reaction counts, video/voice timers, and a few settings readouts (`Messages.module.css`, `VideoPlayer.module.css`, `VoiceNotes.module.css`, `ChannelUsage.module.css`, `AppearanceSettings.tsx`, `DeveloperSettings.tsx`, `ClientMetricsPanel.tsx`, `ChannelTextField.tsx`) |
| Scale | `--type-scale` is 1, 0.9 for `data-font-size="smaller"`, and 1.1 for `"larger"`. The root `font-size` is multiplied by `--buzz-text-scale` (`src/shared/theme/service.ts:73`, `public/appearance-init.js:25`), rounded to 0.1 |

The type ramp (`typography.css:43-92`) defines size / line-height / letter-spacing / weight for each role:

| Role | Size (px) | Line height | Letter-spacing | Weight |
|---|---|---|---|---|
| display | 56 | 1.1 | -0.04em | 400 |
| title | 32 | 1.1 | -0.015em | 500 |
| heading | 24 | 1.1 | -0.0075em | 500 |
| body-lg | 20 | 1.4 | -0.01em | 400 |
| body | 14 | 20px | -0.005em | 400 |
| body-sm | 14 | 20px | -0.0025em | 400 |
| label | 16 | 24px | 0 | 500 |
| label-sm | 14 | 20px | 0.0025em | 500 |
| caption | 12 | 16px | 0.0133em | 400 |
| mono / mono-sm / mono-lg | 12 | 16px | 0.03em | 400, JetBrains Mono |

### 8.3 woff2 locations after install

| Font | Path |
|---|---|
| Inter | `node_modules/@fontsource-variable/inter/files/inter-<subset>-wght-<normal\|italic>.woff2` (symlink to `node_modules/.pnpm/@fontsource-variable+inter@5.3.0/node_modules/@fontsource-variable/inter/files/`). The same folder also has the unused `-opsz-` and `-standard-` variants |
| JetBrains Mono | `node_modules/@fontsource/jetbrains-mono/files/jetbrains-mono-<subset>-400-normal.woff2` (symlink to `node_modules/.pnpm/@fontsource+jetbrains-mono@5.3.0/…/files/`) |

For a GPUI port, the minimum files are `inter-latin-wght-normal.woff2`, `inter-latin-wght-italic.woff2` (plus latin-ext if needed) and `jetbrains-mono-latin-400-normal.woff2`. GPUI/font-kit may need TTF/OTF rather than woff2. In that case, use the upstream variable `Inter[opsz,wght].ttf` / `Inter-Italic[opsz,wght].ttf` and `JetBrainsMono-Regular.ttf`, which have the same family names and license.

### 8.4 Licenses

Both packages declare `"license": "OFL-1.1"` in `package.json` and ship the full SIL OFL 1.1 text.

| Font | Copyright line (from package `LICENSE`) |
|---|---|
| Inter | "Copyright 2016 The Inter Project Authors (https://github.com/rsms/inter) Inter-Italic[opsz,wght].ttf: Copyright 2016 The Inter Project Authors (https://github.com/rsms/inter)". Then "This Font Software is licensed under the SIL Open Font License, Version 1.1." (`node_modules/@fontsource-variable/inter/LICENSE`; also `metadata.json` `license.attribution`) |
| JetBrains Mono | "Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono) JetBrainsMono-Italic[wght].ttf: Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono)". Then the same OFL 1.1 statement (`node_modules/@fontsource/jetbrains-mono/LICENSE`) |

### 8.5 Emoji handling

- **No bundled emoji font.** Emoji render with the system color-emoji font via normal fallback from Inter.
- `--font-emoji` names "Apple Color Emoji", "Segoe UI Emoji" and "Noto Color Emoji" explicitly. Only the video player uses it.
- The canvas avatar renderer uses `'258px "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", sans-serif'` (`src/features/profiles/avatar-upload.ts:73`).
- **Picker:** `emoji-mart` 5.6.0 with `@emoji-mart/data` 1.2.1, `set: "native"` (system glyphs, no sprite sheet) (`src/bundled/emoji/emoji-mart.ts:105-125`).
  - Reaction names come from `@emoji-mart/data/sets/15/native.json` (`src/features/messages/reaction-name.ts:1`).
  - Custom (NIP-30) emoji are images. In reactions they are 14x14 (`.reactionCustomEmoji`, `Messages.module.css:248-253`); in picker cells they are capped at 32px.
- **Reaction sizes:** a native emoji in a reaction is 14px (`--text-body-sm`), line-height 1.2857, min-width 18px.
- **Emoji-only messages** are detected by `src/features/messages/emoji-size.ts` and render as "large emoji". `MessageRow.tsx:380` passes `largeEmoji`.
- **For GPUI:** rely on the platform emoji font fallback, e.g. Noto Color Emoji on Linux.

## 9. Reference screenshots

All PNGs are in `docs/port/screenshots/`. Each name exists as `<name>-light.png`
and `<name>-dark.png` (202 files, 101 pairs). The viewport is 1200x800 at
1x, locale en-US, timezone UTC, headless Chromium 153 (nixpkgs). Names marked
**full** are full-page captures, 1200px wide and cut off at 6000px tall.
Everything else is a single 1200x800 viewport.

### 9.1 Real app shell (built app, synthetic relay)

Sources: `scripts/reference/app-shell.spec.mjs` with buzz-app's own Playwright
fixture `tests/browser/fixture.mjs`. That fixture runs the production build,
the real services and IndexedDB, and a synthetic relay broker. The Primary
community has channels Alpha/Beta and DMs with "Alice/Bob/Carol Fixture".
The viewer is "Fixture Reader". The spec adds six markdown messages (bold,
inline code, link, nested lists, blockquote, strikethrough, a fenced code
block, a table and emoji-only) to Alpha before the page loads.

| Name | Shows |
|---|---|
| `app-shell-no-community` | Shell with no community (plain `pnpm dev` without `BUZZ_DEV_VIEWER`): header, rail (globe, plus), sidebar page rows, "Your channels, one conversation." empty state |
| `app-channel-timeline` | Messages page, #Alpha scrolled to the bottom: day divider, viewer group with the markdown specimens, composer |
| `app-channel-timeline-top` | Same channel scrolled to the top: Alice Fixture group (other author), two day dividers, "Jump to latest" pill |
| `app-message-hover` | Hover action bar on an emoji-only continuation row (hover time "12:17" in the gutter). The fixture can't react, so only Reply / Copy link / More appear |
| `app-thread-panel` | Thread tab opened in the right dock, with its own composer ("Reply in thread to ...") |
| `app-composer-mention` | `@` autocomplete popup above the composer; send button turns prominent |
| `app-composer-channel` | `#` typed: channel autocomplete popup (hash icon rows, "Enter" hint on the highlighted row) |
| `app-composer-emoji` | `:smi` typed: emoji shortcode autocomplete (emoji + `:shortcode:` rows, 7 visible) |
| `app-composer-emoji-picker` | Composer emoji picker popover (search, categories, footer tabs) |
| `app-composer-formatting` | Formatting toolbar replacing the composer toolbar (close, B, I, S, code, code block, link, lists, quote, spoiler) |
| `app-channel-menu` | Channel header "..." menu (also shows the formatting toolbar still open) |
| `app-channel-members` | Channel members dialog (scrim plus blur, search field, member list). The "Invalid channel ID" error is a fixture artifact |
| `app-search-dialog` | Search Buzz palette over the blurred shell |
| `app-channel-beta` | #Beta selected (sparser channel, sidebar selection state) |
| `app-agents` | Agents page in the browser host ("Local agent controls require the desktop app"). See 9.4 for the desktop-style inventory |
| `app-profile-menu` | Profile button menu |
| `app-settings`, `app-settings-appearance`, `app-settings-notifications`, `app-settings-agents` | Settings page: sidebar sections (Profile ... Updates), detail panes |

Known fixture artifacts in these shots (not part of the real UI): the
"Loading Me placement... / Retry Me" block at the top of the sidebar,
"Saved groups and stars aren't supported by this host yet." in the sidebar
footer, and the "Invalid channel ID" text in the members dialog. Channel
display names sometimes show the raw id in lowercase (`Message #alpha`)
before the name resolves.

### 9.2 Message gallery (`tests/fixtures/message-gallery.html?theme=`)

Real `MessageRow`/`MessageMarkdown` renders of every message variant with
sample data. These are the main reference for message visuals.

| Name | Shows |
|---|---|
| `gallery-messages-all` / `gallery-messages-all-full` (full) | All sections (top / full page up to 6000px) |
| `gallery-messages-workflow-attribution` (full) | Workflow/automated messages ("owned by"), info popover trigger |
| `gallery-messages-content-and-identity` (full) | Plain text, agent (squircle) avatar, unknown author, long message, rich text, code and tables, emoji-only, custom emoji, person mention, edited content |
| `gallery-messages-conversation-context` (full) | Date separator and consecutive (grouped) messages |
| `gallery-messages-attachments` (full) | Image attachment, multiple images, unavailable image, video, unavailable video, file, unavailable file |
| `gallery-messages-delivery-states` (full) | Pending/unconfirmed/failed delivery notices, "Unconfirmed with details" |
| `gallery-messages-membership-activity` (full) | Member added/removed system rows, grouped joins |
| `gallery-messages-narrow` | Narrow-preview toggle |

### 9.3 Composer gallery (`tests/fixtures/product-ui.html`)

| Name | Shows |
|---|---|
| `gallery-composer` (full) | Production composer: default, unavailable, read-only, validation/retry and other states |
| `gallery-composer-mention` | Mention popup with two namesake people ("Alice · pktz" / "Alice · rcaj"), public-key suffix |

### 9.4 Dialog atlas and agent fixture

These come from a **production build** of `tests/fixtures/dialog-gallery.html`
and `agent-control.html` (`scripts/reference/vite.fixtures.config.mjs`). In
Vite dev mode these fixtures load CSS in a different order, which drops dialog
padding, so use these builds.

| Name | Shows |
|---|---|
| `dialog-atlas` | Dialog atlas index |
| `dialog-search` | Search Buzz palette |
| `dialog-join` | Add a community (step 1) |
| `dialog-create` | **Add agent** dialog (fields in order: Name, Agent instructions, AI configuration: Harness, Provider, Model, Advanced, Conversation context, Environment ...) |
| `dialog-edit` | Edit agent dialog |
| `dialog-channel`, `dialog-leave`, `dialog-delete`, `dialog-enable`, `dialog-dismiss`, `dialog-secret`, `dialog-webhook`, `dialog-attachment`, `dialog-review` | Other dialogs in the catalog (`tests/fixtures/dialog-gallery/catalog.ts`): channel settings, leave/delete confirmations, workflow dialogs, attachment viewer, review |
| `fixture-agents-page` (full) | Desktop-style Agents inventory: header buttons "Refresh agents" / "Add agent", an agent tile (squircle avatar, name, model), "Other agents (1)", "Import from another installation". The control strip at the top belongs to the fixture. The page is rendered without the shell panel padding |
| `fixture-agents-add-dialog` | Add agent dialog with its left catalog rail ("Create agent") |
| `fixture-agents-manage-dialog` | "Manage {agent}" dialog (relay, process status, Stop) |

### 9.5 Design-system viewer (`just design`, `tests/fixtures/design-system.html`)

All are **full** captures of one viewer page. `design-<x>` maps to route
`#/design/<x>` for foundations and `#/design/components/<x>` for components.

- Foundations: `design-color`, `design-color-table`, `design-typography`,
  `design-icons`, `design-spacing`, `design-radius`, `design-elevation`,
  `design-floating-surfaces`, `design-glass`, `design-motion`.
- Patterns: `design-forms`, `design-messages`.
- Components: `design-components` (index), `design-accordion`,
  `design-alert-dialog`, `design-avatar`, `design-button`, `design-calendar`,
  `design-checkbox`, `design-choice-row`, `design-combobox`, `design-composer`,
  `design-dialog`, `design-empty-state`, `design-field`, `design-field-button`,
  `design-full-page-surface`, `design-header`, `design-icon-button`,
  `design-inline-chip`, `design-input`, `design-keyboard-shortcut`,
  `design-menu`, `design-navigation-item`, `design-navigation-section`,
  `design-panel`, `design-panel-header`, `design-popover`,
  `design-preference-row`, `design-preview-card`, `design-radio-group`,
  `design-search-field`, `design-select`, `design-settings-group`,
  `design-switch`, `design-tabs`, `design-textarea`, `design-theme-picker`,
  `design-toast`, `design-tooltip`.
- Layout playgrounds: `design-workspace`, `design-flex-workspace`,
  `design-swap-workspace`.

Not captured: the guide pages (design guide, agent guide, maintaining, Base
UI backing). They are prose, and the same text is in `DS/DESIGN.md`,
`DS/AGENTS.md` and `DS/MAINTAINING_DESIGN_SYSTEM.md`.

### 9.6 What could not be captured

- **Live-account screens.** Without a `BUZZ_DEV_VIEWER` pin, `just web` has no
  community: `app-shell-no-community` shows what that mode renders. All
  channel screens therefore come from buzz-app's synthetic-relay test fixture.
  That fixture lacks presence, unread badges and typing indicators, and can't
  write reactions, so those states appear only in the message gallery and
  design-viewer specimens. Sections 3 and 4 describe them from source.
- **Desktop-only agent controls.** In a browser, the Agents page only says
  that local agent controls need the desktop app. The inventory, tiles and
  Add/Edit/Manage dialogs come from the isolated `agent-control` fixture and
  the dialog atlas (9.4).
- **Native window chrome.** Tauri window controls (macOS traffic-light inset,
  Linux/Windows min/max/close) only render in the desktop host. Their
  measurements are in section 2.4.
- **Hermit.** The repo's Hermit `bin/just` was not needed. The system node
  24.19 and pnpm 11 (which switched itself to the pinned 11.8.0) ran
  `pnpm install --frozen-lockfile`, and Chromium came from
  `nix build nixpkgs#chromium`.

### 9.7 Re-capturing

```sh
# everything: dev servers, fixture build, fixture/design captures,
# tokens-resolved.json, then the real-app Playwright run
scripts/reference/capture-all.sh
# subsets
scripts/reference/capture-all.sh app                       # real app only
JOBS=designViewer scripts/reference/capture-all.sh fixtures
FILTER='^design-button' JOBS=designViewer scripts/reference/capture-all.sh fixtures
```

Environment overrides: `BUZZ_APP` (default `~/projects/buzz-app`), `CHROMIUM`
(default: `nix build nixpkgs#chromium`), `SHOT_DIR`. The scripts use ports
1531 (web), 1542 (design viewer) and 1561 (static fixture build). The
real-app run builds its own preview server on a free port.
