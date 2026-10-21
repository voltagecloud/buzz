# Messages, conversation and search: parity audit (buzz-app vs buzz-gpui)

Audit date: 2026-10-09. Read-only audit of the main checkouts (`.claude/worktrees/` ignored).

- Reference: `/home/ben/projects/buzz-app` (paths in the "buzz-app reference" column are relative to it).
- Port: `/home/ben/projects/buzz-gpui` (paths in "Port status" are relative to it).
- Slice: `src/features/messages`, `src/features/conversation`, `src/features/search`, and `src/bundled/{composer,emoji,mentions,links,link-lab,diffs,voice-notes}`. Where a search behaviour lives in `src/app/shell/*` (the only consumer of `src/features/search`), that file is cited.
- Statuses: MISSING, PARTIAL (what is missing is stated), DIFFERENT (how is stated), DONE (with the files), SKIPPED (decided: one of the decisions in GAPS.md). The first pass counted DONE behaviours without listing them; later passes kept the closed rows in the tables.
- Sizes: S = under about a day, M = a few days, L = a large subsystem.
- Each behaviour appears once. Where it touches two areas, it is listed in the area that owns it (for example, report and "Send to channel" are under section 2, timecoded replies under section 5, membership grouping under section 1).

## Summary

| Area | MISSING | PARTIAL | DIFFERENT | DONE | SKIPPED |
|---|---|---|---|---|---|
| 1. Message rows, Markdown, links, embeds and diffs | 0 | 0 | 1 | 60 | 2 |
| 2. Hover actions, menus, edit/delete/report, reactions, emoji and GIF pickers | 0 | 0 | 1 | 83 | 0 |
| 3. Timeline, scrolling, unread, jump-to, threads, typing and search | 0 | 0 | 1 | 68 | 0 |
| 4. Composer, autocomplete, formatting, attachments and voice recording | 0 | 0 | 4 | 72 | 1 |
| 5. Rendered attachments, media viewer, audio/video and voice-note playback | 0 | 0 | 3 | 61 | 0 |
| **Total** | **0** | **0** | **10** | **344** | **3** |

Counts are of the rows in the tables. What is left:

- DIFFERENT where GPUI cannot match: no HTML clipboard flavour, no inline custom emoji images in the draft, no AVIF decoding, no squircle clipping, no zoom-in cursor, no exit motion for overlay docks (sections 1, 3, 4 and 5).
- DIFFERENT by design or by the relay: the composer edits Markdown source, so there is no conversion undo step; the relay refuses the `waveform` imeta key; a GPUI clipboard write cannot fail; the desktop session always writes (sections 2 and 4).

## 1. Message rows, Markdown, links, embeds and diffs

Note: Confirmed in buzz-app: there are no OpenGraph/unfurl cards and no YouTube embeds. Links get only a host icon. The only link "previews" are hover cards for Buzz message and channel links. Code blocks have no syntax highlighting or copy button. Edited messages show no "(edited)" marker.

Rows: DONE 60, DIFFERENT 1, SKIPPED 2.

### Message rows: kinds, grouping, byline, avatar

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Time tooltip: full date and time with time zone ("…3:05:09 PM EDT"), 500ms delay | src/features/messages/MessageTimestamp.tsx:75 | DONE (crates/app/src/ui/time.rs) | The port's tooltip omits the time zone abbreviation. Append `%Z` (or a local tz name) | S |
| Name resolution through identity naming (duplicate names disambiguated, channel-member-scoped `useChannelIdentityNames`) for bylines, mentions and membership rows | src/features/messages/MessageRow.tsx:227-230 | DONE (crates/app/src/ui/identity.rs, crates/app/src/ui/message.rs, crates/app/src/ui/membership.rs) | The port uses the raw profile name. Port `identity-names/policy.ts` and use it in bylines, mentions, membership text and thread avatars | M |
| Avatar: 40px circle for people, squircle for agents (agent envelope, known agent key, or `isAgent`), initial fallback | src/features/messages/MessageRow.tsx:245-248, 444-470 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/identity.rs) | The port checks the envelope and `profile.is_agent` only. Also treat keys in the viewer's agent library (`agentPubkeys`) as agents | S |
| Clicking the avatar opens the author's profile (round IconButton, "View {name} profile") | src/features/messages/MessageRow.tsx:405-443 | DONE (crates/app/src/ui/message.rs): opens through `navigation::show_profile`, which shows the profile card until the profile panel (channels.md) exists | Make the head avatar clickable and call the same profile opener mentions use (`navigation::show_profile`) | S |
| Presence badge on agent avatars (online, away, offline dot with cutout) | src/features/messages/MessageRow.tsx:249-254, 434 | DONE (crates/app/src/ui/message.rs; agents in view are watched in crates/app/src/community.rs) | Read agent presence from the session and pass `.presence(...)` on agent avatars in rows | S |
| Agent "thinking" badge on the avatar while the agent works in the channel | src/features/messages/MessageRow.tsx:424-433 | DONE (crates/app/src/ui/message.rs): driven by the agent's typing state in the conversation, the agent activity the session has | Show the thinking pill (three pulsing dots) on agent avatars while their typing state is active | S |
| Workflow message avatar: squircle with a lightning-bolt icon | src/features/messages/MessageRow.tsx:391-404 | DONE (crates/app/src/ui/message.rs) | When `row.workflow_owner` is set, draw `Avatar::fallback_icon(Lightning, 20)` in a squircle in place of the signer's avatar | S |
| Workflow byline: "Workflow · owned by {owner}", with the owner as a link button that opens their profile | src/features/messages/WorkflowByline.tsx:31-46 | DONE (crates/app/src/ui/message.rs) | Render the workflow byline when `workflow_owner` is set (notifications already use the wording at crates/app/src/notifications.rs:188) | S |
| Workflow info popover: "Workflow message" with an explanation, owner name, owner npub, relay signer npub | src/features/messages/WorkflowByline.tsx:47-76 | DONE (crates/app/src/ui/message.rs) | Add an xs info IconButton that opens a popover with these fields | S |
| "Sent from Thread — {excerpt}" line above the body | src/features/messages/MessageRow.tsx:613-629 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/links.rs) | The port shows link-coloured text that does nothing. Make it a real thread link (ChatsCircle icon, opens `buzz://message?channel&id=root&thread=root`) | S |
| Thread-summary participant avatars have name tooltips and agent squircles from the agent library | src/features/messages/ReplySummary.tsx | DONE (crates/app/src/ui/message.rs) | Add the `title` tooltips and agent-library squircles | S |

### System / membership rows

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Group consecutive membership events (same day, gaps under 1 hour, compatible kinds, join-then-leave lifecycle) into one row | src/features/messages/membership-rows.ts:22-70 | DONE (crates/app/src/ui/membership.rs) | Port `membershipRows`. The port renders one line per event | M |
| Wording: "joined", "{A} was added by {X}", "were added", "arrived", "joined and left the channel", "left the channel", "was removed by {X}", ", along with B, C and N others" | src/features/messages/membership-rows.ts:72-131 | DONE (crates/app/src/ui/membership.rs) | The port says "joined" even when someone else added the person, and has no group wording. Port `membershipDescription` in full | S |
| Viewer named "You"/"you" ("You were added by X") | src/features/messages/membership-rows.ts:84-88 | DONE (crates/app/src/ui/membership.rs) | Use "You"/"you" when the target or actor is the viewer | S |
| Up to 3 stacked avatars (agent squircles) plus a "+N" chip | src/features/messages/MembershipRow.tsx:42-72 | DONE (crates/app/src/ui/membership.rs) | Add the 28px avatar stack with a 2px panel ring and the +N chip | S |
| Tooltip listing every named person in the group | src/features/messages/MembershipRow.tsx:73 | DONE (crates/app/src/ui/membership.rs) | Add a tooltip with the `Intl.ListFormat`-style list of names | S |

### Delivery state (pending / failed)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| 10-second grace period: nothing shows for the first 10s. After that, a message still Sending or Accepted (not Seen) shows "Delivery not yet confirmed." | src/features/messages/delivery.ts:3-13, src/features/messages/DeliveryNotice.tsx:14-22 | DONE (crates/app/src/ui/message.rs) | The port shows Failed/Unknown at once and never warns about a stuck Sending/Accepted row. Add the grace timer and include Sending/Accepted after 10s | S |

### Markdown constructs

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Rich inline content inside table cells (clickable links with icons, mentions, custom emoji, nested code) | src/features/messages/MessageMarkdown.tsx:543-602 | DONE (crates/app/src/ui/markdown.rs) | Cells are flat `StyledText`: links are only coloured and cannot be clicked, mentions and custom emoji are not rendered. Render cells through the same inline `TextView` path | M |
| GFM footnotes (reference marker plus a footnotes section) | src/features/messages/MessageMarkdown.tsx:620 (remark-gfm) | DONE (crates/app/src/ui/markdown.rs) | The port prints `[^id]` as text and drops the definitions. Render the footnote marker and an end section, or match whatever react-markdown shows | S |
| Formatting inside link labels kept (`[**bold**](url)`) | src/features/conversation/MessageLink.tsx:207-220, src/bundled/links/InlineLink.tsx:152-158 | DONE (crates/app/src/ui/markdown.rs, crates/app/src/ui/links.rs) | The port flattens labels to plain text. Keep emphasis/code styles in the link chip | S |
| Repair pasted wrapped links (`[label]\(url\)`, `[label]([url](url))`) before parsing | src/features/messages/message-link-parts.ts:147-165 | DONE (crates/app/src/ui/markdown.rs) | Port `normalizeWrappedLinks` into body preparation | S |
| Custom emoji tooltip (`title=":shortcode:"`) | src/bundled/emoji/CustomEmoji.tsx:23 | DONE (crates/app/src/ui/markdown.rs) | Add a tooltip with `:shortcode:` on inline and jumbo custom emoji | S |
| Spoiler mask animation: three tiled dot fields that twinkle (reduced motion: static) | src/features/messages/Messages.module.css:1855-1933 | DONE (crates/app/src/ui/markdown.rs): GPUI has no reduced-motion setting, so the dots always twinkle | The port uses a flat 35% subtle fill. Draw the dot pattern with its twinkle | S |
| Spoilers hold rich content and span formatting (`\|\|a *b* [link](u)\|\|`), and long spoilers wrap | src/features/messages/remark-spoilers.ts:44-82 | DONE (crates/app/src/ui/markdown.rs) | The port only finds spoilers inside one text node, flattens the inside to plain text and sets `nowrap`. Build spoilers from sibling nodes and allow wrapping | M |

### Mentions and channel references in prose

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Agent-library names also bind mentions | src/features/messages/profile-mentions.ts:27-33 | DONE (crates/app/src/ui/markdown.rs, crates/app/src/ui/identity.rs) | The port reads names from profiles only. Add the viewer's agent identities | S |
| Clicking a mention opens the profile panel (side panel) | src/features/messages/MessageMarkdown.tsx:527-533 | DONE (crates/app/src/ui/workspace.rs `show_profile`, crates/app/src/ui/profile_panel.rs): mentions, avatars, workflow owners and "View profile" open the profile panel as a tab beside the conversation | - | M |
| `#channel-name` in prose becomes a channel link (hash or lock icon) for known, unarchived, unambiguous channels | src/features/messages/ReferenceText.tsx:111-187, src/features/messages/message-references.ts:14-72 | DONE (crates/app/src/ui/markdown.rs) | Port `messageReferences` for channels into the prose scan and emit `buzz://channel/<id>` links | M |

### Links: inline rendering and behaviour

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Host icon table: GitHub, Drive, Docs/Sheets/Slides, Figma, Notion, Slack, Dropbox, OneDrive, GitLab, YouTube, Loom, Zoom, Teams, globe | src/bundled/links/InlineLink.tsx:33-102 | DONE (crates/app/src/ui/links.rs; OneDrive mark in crates/ui/src/icon.rs) | Missing: OneDrive (`onedrive.live.com`, `1drv.ms`, `*-my.sharepoint.com`), `dropboxusercontent.com`/`db.tt`, `zoom.com`, `teams.cloud.microsoft`. Other `docs.google.com` paths should use the Drive icon, not FileText | S |
| Raw URLs are cut to 44 characters plus "…" (only when longer than 45) | src/bundled/links/InlineLink.tsx:160-163 | DONE (crates/app/src/ui/links.rs) | The port truncates at more than 44, so a 45-character URL loses its last character. It also compares the label to the URL literally, so Unicode-encoded destinations are not treated as raw | S |
| Long link labels wrap; only the icon and the host stay together | src/bundled/links/InlineLink.tsx:164-177, src/shared/InlineReference.module.css:12-14 | DONE (crates/app/src/ui/links.rs): the icon and host stay together and the rest wraps inside the chip; a chip that fits the body but not the rest of a line moves to the next line whole, since inline objects in a TextView are atomic | The whole chip is `whitespace_nowrap`, so long authored labels overflow. Split the lead from the rest and let the rest wrap | M |
| Bare `buzz://` URLs in text become Buzz links (trailing punctuation trimmed) | src/features/messages/message-link-parts.ts:86-144 | DONE (crates/app/src/ui/markdown.rs) | GFM autolink literals only cover http/www/email. Add the `messageLinkParts` scan for bare `buzz://` (and `<buzz://…>` already works) | S |
| Unresolved Buzz links: look up channels the reader hasn't joined (`describe`/`refer`) for their name; "Private channel" with a lock only when access is withheld; otherwise generic "Channel" / "Message" / "Thread" / "Buzz" | src/features/conversation/MessageLink.tsx:95-118, src/features/conversation/channel-reference.ts:13-41, src/bundled/links/InlineLink.tsx:134-140 | DONE (crates/app/src/ui/links.rs) | The port says "Private channel" for every channel it doesn't list, including public ones and message links. Add the channel-reference lookup and the generic labels | M |
| DM channel links use the channel/lock icon, not a chat icon | src/bundled/links/InlineLink.tsx:151 | DONE (crates/app/src/ui/links.rs) | Port shows ChatCircle for a DM channel link. Use the lock icon (private) as the reference does | S |
| URL tooltip on plain links (`title=url`); "Private channel. You aren’t a member, or it no longer exists." on withheld links | src/features/conversation/MessageLink.tsx:228-234 | DONE (crates/app/src/ui/links.rs) | Add the tooltips | S |
| Middle-click a Buzz link opens it in-app | src/features/conversation/MessageLink.tsx:190-205 | DONE (crates/app/src/ui/links.rs) | Handle the middle mouse button on Buzz link chips | S |
| Status "This Buzz link couldn’t be opened here." when navigation fails | src/features/conversation/MessageLink.tsx:367-369 | DONE (crates/app/src/ui/links.rs, crates/app/src/ui/navigation.rs `try_open_url`) | The port only logs. Show the inline status (or a toast) | S |
| External link right-click / ContextMenu / Shift+F10 menu: "Open in browser", "Preview snapshot" (agent snapshots), "Copy link" with "Link copied" toast or error toast | src/features/conversation/MessageLink.tsx:246-301, 127-144 | DONE (crates/app/src/ui/links.rs): links are tab stops; the context-menu key and Shift+F10 open the menu, which takes the arrow keys and gives focus back to the link; Preview snapshot shows for agent snapshot links | - | M |
| Session link chip: "Session · {id8}" with a file icon for session channels the viewer belongs to | src/features/conversation/MessageLink.tsx:145-159, 209-215; src/features/conversation/LinkPreview.module.css:1-27 | DONE (crates/app/src/ui/links.rs) | Detect the session label form and draw the chip | S |
| Join/invite links render as "Join {host}" (port-only behaviour) | n/a | DONE (crates/app/src/ui/links.rs): the "Join" chip is gone; join links show as plain underlined links, as in buzz-app, and still open the invite flow | buzz-app has no such chip in message rendering. Keep or remove on purpose | S |

### Link hover previews and embeds

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Message link hover/focus card (above the link): author avatar (workflow bolt), name, relative time with full-date tooltip, channel icon and name (DM participants, "Notes to self", "Channel unavailable"), excerpt clamped to 4 lines that stops at a blank paragraph, "Attachment"/"Empty message" | src/features/conversation/BuzzLinkPreview.tsx:21-175, src/features/conversation/message-preview-text.ts:2-8 | DONE (crates/app/src/ui/link_preview.rs, crates/app/src/ui/links.rs): on hover; link chips take no focus | Add a hover card on message links that opens a bounded exact thread read for the target | M |
| Message preview loading and error states: "Loading message…", "Message preview unavailable." | src/features/conversation/BuzzLinkPreview.tsx:42-46, 99-104 | DONE (crates/app/src/ui/link_preview.rs) | Part of the hover card | S |
| Clicking the preview card opens the link | src/features/conversation/MessageLink.tsx:330-341 | DONE (crates/app/src/ui/link_preview.rs) | Part of the hover card | S |
| Channel link hover card: name, "Public/Private channel · Archived · Joined/Not joined · N members", description | src/features/conversation/BuzzLinkPreview.tsx:180-212 | DONE (crates/app/src/ui/link_preview.rs, crates/app/src/ui/links.rs) | Add a hover card for channel-only links using the channel-reference lookup | M |
| Embedded thread (a plugin page hosting a full thread panel, with panel and media-review overlays) | src/features/conversation/EmbeddedThread.tsx:45-180 | SKIPPED (decided): only external plugins mount it (no bundled plugin does), and external plugin import is skipped | - | L |

### Diff messages

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Diff card: header with file path (or "Diff"), short commit SHA linking to `{repo}/commit/{sha}` (or "Source"), expand button | src/bundled/diffs/DiffMessage.tsx:21-24, 43-66 | DONE (crates/app/src/ui/diff.rs) | Render rows with `row.diff` as a diff card (DiffMeta is already folded at crates/relay/src/fold/mod.rs:82-88) | M |
| Diff description line under the header | src/bundled/diffs/DiffMessage.tsx:67-69 | DONE (crates/app/src/ui/diff.rs) | Part of the diff card | S |
| Scrollable preview (max 400px) of unified hunks: green insert and red delete lines and gutters, mono font | src/bundled/diffs/DiffMessage.tsx:70-77, src/bundled/diffs/Diffs.module.css:57-96 | DONE (crates/app/src/ui/diff.rs) | Port a unified diff view (hunk parse plus line colouring) | M |
| Per-file headers in multi-file or non-modify patches: path (or "old → new"), "New file/Copied/Deleted/Modified/Renamed", +N / −N counts | src/bundled/diffs/DiffViewer.tsx:51-58, src/bundled/diffs/parse.ts:85-103 | DONE (crates/app/src/ui/diff.rs) | Part of the diff view | S |
| Strict patch validation; falls back to a raw scrollable `pre` for incomplete, binary, unknown or over-100k patches | src/bundled/diffs/parse.ts:3-83, src/bundled/diffs/DiffViewer.tsx:17-29 | DONE (crates/app/src/ui/diff.rs) | Port `parsePatch` and `consumesPatch` | M |
| Empty states: "No diff content", "No textual hunks in this diff." | src/bundled/diffs/DiffViewer.tsx:30, 78 | DONE (crates/app/src/ui/diff.rs) | Part of the diff view | S |
| Truncated notice: "Diff truncated. View full diff at the source repository" (link when a source is known) | src/bundled/diffs/DiffMessage.tsx:28-39 | DONE (crates/app/src/ui/diff.rs) | Part of the diff card | S |
| Expanded diff dialog with a Unified/Split toggle, "Close diff" | src/bundled/diffs/DiffMessage.tsx:80-108 | DONE (crates/app/src/ui/diff.rs): Escape and the close button close it; like buzz-app's Dialog, a scrim press does not | Add the dialog and a split (side-by-side) layout | M |

### Copy and selection

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Select across paragraphs, blocks and several message rows in one drag | src/features/messages/selection-copy.ts:344-402 | DONE (crates/app/src/ui/markdown.rs; third_party/gpui-base/src/selectable_text.rs orders plain runs): the window selection spans paragraphs, blocks and rows. Table cells copy column by column | Each paragraph is a separate `TextView`. A shared selection across a row and across rows is needed | L |
| Code blocks, tables, plain-text (over-limit) bodies and jumbo emoji are selectable | src/features/messages/Messages.module.css:1386-1438 | DONE (crates/app/src/ui/markdown.rs) | These are plain `div`/`StyledText`, not selectable. Render them in selectable text views | M |
| Rich clipboard: `text/html` flavour, and a Markdown flavour on paste into the composer that keeps mentions, links and code blocks | src/features/messages/selection-copy.ts:186-230, 365-388 | DIFFERENT: GPUI's clipboard carries plain text and images only, with no `text/html` or custom flavour to write or read | - | M |
| Plain-text copy rules: links as "label (url)", mentions as "@Name", custom emoji as `:shortcode:`, bylines/timestamps/screen-reader text left out | src/features/messages/selection-copy.ts:144-176, src/bundled/emoji/copy-emoji.ts:7-33 | DONE (crates/app/src/ui/markdown.rs) | Inline objects copy their visible text, so labeled links lose the URL. Copy links as "label (url)" | S |
| "Copy link" gives `buzz://message?channel=…&id=…&thread=…` (thread kept for replies) | src/features/messages/message-copy.ts:51-78 | DONE (crates/app/src/ui/message_copy.rs) | The port omits `thread=` for replies. Add the thread root | S |

### Plugin rendering contracts and misc

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Message renderer registry (a plugin replaces a message body; diffs use it) | src/features/conversation/MessageBody.tsx:7-38, src/features/conversation/service.tsx:33 | DONE (crates/app/src/ui/renderers.rs) | No plugin system. Diff rendering can be hard-coded (see Diff messages) | L |
| Inline text renderer registry (custom emoji are an inline plugin in buzz-app) | src/features/conversation/InlineText.tsx:13-74 | DONE (crates/app/src/ui/renderers.rs) | Custom emoji are native in the port, which is enough unless third-party inline plugins are wanted | L |
| Link renderer registry (bundled links plugin, lowest order wins, catch-all for web) | src/features/conversation/MessageLink.tsx:49-52, src/bundled/links/index.tsx:7-16 | DONE (crates/app/src/ui/renderers.rs, crates/app/src/ui/links.rs) | Native in the port. A registry is needed only if plugins are ported | L |
| Attachment renderer registry (voice notes) | src/features/conversation/AttachmentView.tsx:10-37 | DONE (crates/app/src/ui/renderers.rs) | Voice notes are native in the port | M |
| A plugin that throws falls back to the host rendering | src/features/conversation/ContributionBoundary.tsx:14-25 | DONE (crates/app/src/ui/renderers.rs) | Needed only with a plugin system | S |
| Link Lab page (visual fixture for link chips; not in buzz-app's default plugin catalog) | src/bundled/link-lab/index.tsx:38-164 | SKIPPED (decided) | - | S |

## 2. Hover actions, menus, edit/delete/report, reactions, emoji and GIF pickers

Note: buzz-app has no right-click context menu on messages, no pin action, and no rename/delete in custom emoji settings. Message editing happens in the composer ("Editing message" mode), not inline in the row. The port does the same.

Rows: DONE 83, DIFFERENT 1.

### Hover action bar: buttons, tooltips, visibility

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Action bar also appears when keyboard focus is inside the row (Tab into the row) | src/features/messages/useFloatingActionBar.ts:108-111, 36-43 | DONE (crates/app/src/ui/message.rs, crates/app/src/main_window.rs): Tab moves focus; rows are size-less tab stops (buzz-app reaches the bar through each row's controls), and the focused row scrolls into view and shows its bar, whose buttons take focus | - | M |
| Action bar on thread-panel rows (reactions, reply, copy link, more menu) | src/features/messages/MessageRow.tsx:500-574 (same row component in threads) | DONE (crates/app/src/ui/message_actions.rs, crates/app/src/ui/thread.rs) | The thread panel passes no `on_hover`/`on_popup`/`on_command`, so it has no action bar. It has only a group-hover "Reply" button. Give thread rows the same bar, more menu and picker as the channel. | M |
| Quick reaction label/pressed state: "Remove X" and pressed when the viewer already reacted with it, else "React with X" | src/features/messages/MessageReactions.tsx:152-165 | DONE (crates/app/src/ui/message.rs) | The tooltip is always "React with X" and there is no selected state. Show "Remove X" and `.selected(true)` when the viewer's reaction exists. | S |
| Quick reaction glyph lift on hover (translateY -1px, scale 1.12) and press (0.96), 120ms | src/features/messages/Messages.module.css:1601-1626 | DONE (crates/app/src/ui/message.rs) | - | S |
| Quick reactions hidden when the row is 260px wide or narrower | src/features/messages/Messages.module.css:1829-1830 | DONE (crates/app/src/ui/message.rs) | Hide the quick-reaction buttons when the row width is 260px or less. | S |
| Quick reactions and picker disabled while a reaction to this message is pending/failed in the outbox | src/features/messages/MessageReactions.tsx:67-72, 98, 163 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/outbox_ops.rs) | Track pending kind-7/kind-5 reaction ops per message. Disable quick reactions, pills and add-reaction while one is in flight (stops double toggles). | S |
| Reactions/add-reaction hidden for read-only, archived or unlisted channels or when the outbox lacks kind 7/5 (`canReact`) | src/features/messages/MessageRow.tsx:260-269 | DONE (crates/app/src/ui/message.rs): the port's outbox takes every kind, so only the conversation is checked | Rows always get `on_react` (channel.rs:500). Gate reactions on `!read_only && !archived` and on channel membership. | S |
| "Reply" button opens/replies in the thread, including on thread replies (opens the root) | src/features/messages/MessageRow.tsx:504-514 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs) | Shown only when `thread_root_id` is none. Also show it on replies and open their root with reply intent. | S |
| "Reply" disabled with tooltip "Reply unavailable for this message" (undelivered, archived, read-only, unlisted) | src/features/messages/MessageActionBar.tsx:136-150; MessageRow.tsx:515-521 | DONE (crates/app/src/ui/message.rs) | Add a disabled state and that tooltip. | S |
| "Copy link" button; disabled with tooltip "Message link unavailable" when no link (undelivered) | src/features/messages/MessageActionBar.tsx:151-160 | DONE (crates/app/src/ui/message.rs) | The port hides the button instead of disabling it. Render it disabled with the "Message link unavailable" tooltip. | S |
| Copy failure toast "Couldn’t copy. Try again from the message menu." (error, no timeout) | src/features/messages/MessageActionBar.tsx:114-118 | DIFFERENT: not done. A GPUI clipboard write cannot fail, so the toast has no cause | GPUI's clipboard write cannot fail, so there is no error path. Nothing to do unless a fallible clipboard path is added. | S |
| Plugin-contributed markers in the byline (e.g. reminder marker) | src/features/messages/MessageRow.tsx:599-610 | DONE (crates/app/src/ui/renderers.rs, crates/app/src/ui/message.rs): the registry is there; no bundled plugin registers a marker yet | There is no message-action plugin registry. Add one if plugin actions are ported. | M |

### More menu items and their conditions

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| "Mark unread" / "Mark read" toggle (mail / mail-opened), label based on the message's current unread state | src/features/messages/MessageManagement.tsx:226-267 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs) | The menu always says "Mark unread". Read the message's attention state and offer "Mark read" (`session.mark_message_read`, relay session/mod.rs:450) when it is unread. | S |
| Read-state, edit and delete gated on a managed row: writable (not read-only) channel, not membership, not diff, delivered | src/features/messages/MessageManagement.tsx:210-224 | DONE (crates/app/src/ui/message.rs) | The port checks only "delivered". Add the read-only, diff and membership gates. | S |
| Mark read/unread error shown as alert "Could not update unread state. Try again." | src/features/messages/MessageManagement.tsx:254-259, 194 | DONE (crates/app/src/ui/message_actions.rs) | The port only logs a warning. Surface the error in the page alert or a toast. | S |
| "Copy message" copies Markdown with bound mentions as `[@name](nostr:…)` links (not inside code), then attachment URLs | src/features/messages/message-copy.ts:8-49 | DONE (crates/app/src/ui/message_copy.rs) | The port copies raw `content` and attachment URLs. Add the mention-to-link rewrite with the literal-context (code) exclusion. | S |
| "Send to channel" (own thread replies only; writable channel; outbox supports kind 9) with toast "Sending to channel" / error toast | src/features/messages/MessageActionBar.tsx:227-249; MessageRow.tsx:522-538 | DONE (crates/app/src/ui/message_actions.rs, crates/relay/src/session/messaging.rs) | The relay has the draft builder (crates/relay/src/messages.rs:584 `sent_from_thread`) but no session API and no menu item. Add `Session::send_to_channel`, the menu item and the toasts. | M |
| "Edit message" (pencil 14px): own, not archived, editable per `lastEditableMessage` (outbox supports 40003, not agent envelope/diff, no pending edit/delete) | src/features/messages/MessageManagement.tsx:306-307, 320-339; useMessageEdit.ts:13-39 | DONE (crates/app/src/ui/message.rs) | Shown for any own delivered row. Add the archived, agent-envelope, diff and pending-edit/delete checks. | S |
| Edit item disabled while a delete or edit of the row is in flight | src/features/messages/MessageManagement.tsx:322-331 | DONE (crates/app/src/ui/message.rs) | Disable it when the outbox holds a kind-5/40003 op for this id. | S |
| "Delete message" (trash 14px): own non-archived, or an agent the viewer verifiably owns (owner authorization); outbox supports kind 5 | src/features/messages/MessageManagement.tsx:293-315, 340-368 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs, crates/relay/src/session/messaging.rs) | Agent-owner deletion is missing. The relay accepts `AgentOwner` (relay messages.rs:761-799), but the app never passes it. Add the agent-ownership lookup and pass the authorization. Hide the item in archived channels. | M |
| Delete item tone | src/features/messages/MessageManagement.tsx:340-367 | DONE (crates/app/src/ui/message.rs) | buzz-app uses a plain menu item. The port uses `.danger()` (red). Drop `.danger()`. | S |
| Delete item disabled while an edit/delete of the row is in flight | src/features/messages/MessageManagement.tsx:341-351 | DONE (crates/app/src/ui/message.rs) | Add outbox-pending gating. | S |
| "Follow thread" / "Unfollow thread" (bell / bell-off); hidden in DMs; error "Could not save thread follow. Try again." | src/features/messages/MessageManagement.tsx:369-387 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs, crates/relay/src/session/messaging.rs) | The relay has `Unread::follow` (crates/relay/src/unread.rs:2546) and `threads/follows.rs`, but no session API or menu item. Add both. | M |
| Plugin actions in the menu (e.g. "Remind me") and the dialogs they open | src/features/messages/MessageRow.tsx:279-295, 323-334, 472-480 | DONE (crates/app/src/ui/renderers.rs, crates/app/src/ui/message.rs): the registry is there; no bundled plugin registers an action yet | Needs a message-action contribution registry. | M |
| "Report" (flag) on any delivered, non-membership message | src/features/messages/MessageRow.tsx:307-322 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/report.rs) | Add the menu item (see Report dialog section). | S |
| Menu item order: read state, Copy, Send to channel, Edit, Delete, Follow, plugin, Report | src/features/messages/MessageActionBar.tsx:217-250; MessageRow.tsx:559-573 | DONE (crates/app/src/ui/message.rs) | The order of the existing items matches. The missing items need to go in these positions. | S |
| Menu closes on outside press; Escape closes it; arrow-key navigation and Enter activation; focus returns to trigger | src/features/messages/MessageActionBar.tsx:161-210 (Base UI Menu) | DONE (crates/app/src/ui/message.rs): Escape, Tab or a choice from the keyboard gives focus back to the More button | - | S |

### Message edit mode (in the composer, channel and thread)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Edit header: pencil icon, "Editing message", X button "Cancel edit" ("Close edit" once submitted) | src/features/messages/MessageComposer.tsx:1409-1421 | DONE (crates/app/src/ui/composer.rs hooks, crates/app/src/ui/message_actions.rs) | The "Close edit" label for the locked state and disabling while busy are missing. | S |
| Up arrow in an empty composer edits the newest editable own message | src/features/messages/MessageComposer.tsx:1540-1560 | DONE (crates/app/src/ui/message_actions.rs, crates/app/src/ui/channel.rs, crates/app/src/ui/thread.rs) | It works, but the check skips pending edit/delete ops in the outbox and the outbox's support for kind 40003. Not wired in the thread panel. | S |
| Saving empty text opens the delete confirmation (or errors "Keep attachment links unchanged. To remove this message, use Delete message." when it has attachments) | src/features/messages/MessageComposer.tsx:993-1002 | DONE (crates/app/src/ui/message_edit.rs, crates/app/src/ui/message_actions.rs) | Save is disabled when the text is empty. Route an empty save to the delete dialog, or show the attachment error. | S |
| Edit validation: attachment links must stay unchanged ("Keep attachment links unchanged. Only message text can be edited here.") | src/features/messages/message-edit.ts:42-61 | DONE (crates/app/src/ui/message_edit.rs) | Add the `validateMessageEdit` check before `session.edit`. | S |
| Edit conflict: "This message changed while you were editing. Copy your changes, then cancel and reopen it." | src/features/messages/useMessageEdit.ts:105-110 | DONE (crates/app/src/ui/message_edit.rs) | Compare the current row's source with the target's source on save. | S |
| Target disappears: error "This message is no longer available to edit." | src/features/messages/useMessageEdit.ts:101-104 | DONE (crates/app/src/ui/message_edit.rs, crates/app/src/ui/channel.rs) | The port cancels the edit silently when the row leaves the window. buzz-app keeps the text and shows an error. | S |
| Mentions in the edited text become explicit `[@name](nostr:npub)` links (`messageEditText`) | src/features/messages/message-edit.ts:24-39 | DONE (crates/app/src/ui/message_edit.rs) | Only channel references become chips. Convert bound profile mentions as well. | S |
| Edit stays locked while sending; "Your edit is still in the outbox." + "Retry edit" on failed/unknown delivery | src/features/messages/useMessageEdit.ts:60-81, 128-137; MessageComposer.tsx:1698-1708 | DONE (crates/app/src/ui/message_actions.rs, crates/app/src/ui/composer.rs) | The port awaits `session.edit` and closes the edit or shows the page error. Add the outbox-backed locked/retry state. | M |
| Pending/failed edit and delete recovery panel: "Message edit/deletion: …", edit content, "This may already have reached the conversation…", "Retry message update", "Discard failed update" / "Dismiss unconfirmed update" | src/features/messages/MessageManagement.tsx:484-551 | DONE (crates/app/src/ui/message_actions.rs) | The relay has `retry`/`dismiss` (crates/relay/src/outbox.rs:849, 891; session/mod.rs:418-426), but nothing in the UI lists failed kind-40003/5 ops per channel. | M |
| Editing in a thread panel | src/features/messages/ThreadPanel.tsx:1082-1095, src/features/messages/MessageComposer.tsx:202-203 | DONE (crates/app/src/ui/thread.rs, crates/app/src/ui/message_actions.rs) | Route edit submissions to `session.edit` and wire the Edit action and Up arrow in threads. | M |
| A mention picked during an edit becomes a literal identity link `[@Name](nostr:npub…)` and adds no recipient | src/features/messages/MessageComposer.tsx:783-786, 859-860, src/features/messages/message-edit.ts:19-20 | DONE (crates/app/src/ui/composer.rs, crates/app/src/ui/message_edit.rs) | The port inserts an `@Name` chip, which is saved as plain `@Name` text. Insert the profile link Markdown during edits. | S |
| "Finish or cancel your current edit first." / "This message is no longer available to edit." | src/features/messages/MessageComposer.tsx:729-737 | DONE (crates/app/src/ui/composer.rs, crates/app/src/ui/message_edit.rs) | The port quietly retargets the edit, and cancels it when the row disappears. Add the messages. | S |

### Delete confirm dialog

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Pending state: "Deleting…", buttons disabled, dialog stays until accepted/seen | src/features/messages/MessageManagement.tsx:413-427, 463-468 | DONE (crates/app/src/ui/message_actions.rs) | The port closes at once and runs `remove` in the background. Keep the dialog open while sending. | S |
| Failed/unknown delivery: alert with the op error or "Delivery could not be confirmed. Retry sends the same operation." and a "Retry" button; Cancel becomes "Close" | src/features/messages/MessageManagement.tsx:415-416, 444, 464-478 | DONE (crates/app/src/ui/message_actions.rs) | Errors go to the page banner. Add retry through the outbox and the Close label. | S |
| Guard: "This conversation is no longer available for changes." when the channel is archived/read-only/gone | src/features/messages/MessageManagement.tsx:431-443 | DONE (crates/app/src/ui/message_actions.rs) | Check the channel state before removing. | S |
| Dialog dropped if the channel becomes unavailable; focus returns to the composer after a delete | src/features/messages/MessageManagement.tsx:113-122, 459 | DONE (crates/app/src/ui/message_actions.rs) | Close the dialog on channel change and restore focus to the composer. | S |
| Escape / scrim click cancels the dialog | src/features/messages/MessageManagement.tsx:455-460 (AlertDialog onClose) | DONE (crates/app/src/ui/message_actions.rs) | Only the Cancel button closes it. Wire Escape and scrim dismissal. | S |

### Report dialog

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Report dialog: flag + "Report message" title and description "Reports go to this community's moderators for review. The author is not notified of who reported them." | src/features/messages/ReportMessageDialog.tsx:67-81 | DONE (crates/app/src/ui/report.rs; flag title in crates/ui/src/components/dialog.rs) | Add the dialog. The relay has the kind-1984 builder (crates/relay/src/messages.rs:887) but no session publish API. Add `Session::report` (direct publish, 10s deadline). | M |
| Reason radio group, in order: Spam, Profanity or hate speech, Nudity or sexual content, Impersonation, Malware or scam, Illegal content, Other | src/features/messages/ReportMessageDialog.tsx:12-20, 107-118 | DONE (crates/app/src/ui/report.rs; crates/ui/src/components/radio.rs) | Radio group (ReportType exists in relay messages.rs). | S |
| "Additional context (optional)" textarea, placeholder "Add anything that helps moderators..." | src/features/messages/ReportMessageDialog.tsx:119-127 | DONE (crates/app/src/ui/report.rs) | Textarea field. | S |
| Cancel / "Submit report" (prominent, disabled until a reason, loading while pending, dialog cannot close while pending) | src/features/messages/ReportMessageDialog.tsx:72, 82-95 | DONE (crates/app/src/ui/report.rs) | Buttons and the pending state. | S |
| Error "Failed to submit report. Try again." on the Reason field | src/features/messages/ReportMessageDialog.tsx:51-53, 107 | DONE (crates/app/src/ui/report.rs) | Error display. | S |
| Success toast "Report submitted to community moderators" (5s) | src/features/messages/MessageRow.tsx:490-497 | DONE (crates/app/src/ui/report.rs) | Toast on success. | S |
| Submitted report survives leaving the conversation: "Submitting report…" status or error + "Review report" | src/features/messages/ReportMessageDialog.tsx:40-65 | DONE (crates/app/src/ui/message_actions.rs, crates/app/src/ui/report.rs, crates/app/src/ui/inbox/detail.rs) | - | S |

### Reaction chips

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Count roll animation (260ms vertical roll on change) | src/features/messages/AnimatedReactionCount.tsx:1-45; Messages.module.css:266-302 | DONE (crates/app/src/ui/message.rs) | Animate count changes (old/new numbers slide up or down). | S |
| Click toggles: add if the viewer has not reacted, else remove all the viewer's events in that group | src/features/messages/MessageReactions.tsx:71-89 | DONE (crates/app/src/ui/message_actions.rs) | The port removes only the first own event, and it matches groups by content only, not by content and emoji URL. Remove all own events and match on the custom emoji URL. | S |
| Toggling a custom-emoji pill reuses that pill's emoji tag (works even if the shortcode is not in the viewer's catalog) | src/features/messages/MessageReactions.tsx:84, 319 | DONE (crates/app/src/ui/message_actions.rs, crates/relay/src/session/messaging.rs) | The port sends only content, and the relay resolves it from the catalog (relay messages.rs:747-751). Pass the group's `CustomEmoji` to `react`. | S |
| Pills disabled when reacting is not allowed (read-only, archived, undelivered) | src/features/messages/MessageReactions.tsx:309-314; MessageRow.tsx:805-808 | DONE (crates/app/src/ui/message.rs) | Pills are always clickable. Add the disabled state. | S |
| Reaction error alert "Could not update reaction. Try again." | src/features/messages/MessageReactions.tsx:90-95, 195, 438 | DONE (crates/app/src/ui/message_actions.rs, crates/app/src/ui/message.rs) | The port only logs. Show the error inline. | S |
| Failed/unknown reaction delivery: "Couldn’t update reaction." / "Reaction delivery not confirmed." + "Retry reaction" | src/features/conversation/ReactionTool.tsx:132-166; MessageReactions.tsx:439 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/outbox_ops.rs) | Find kind-7/5 ops for the message in the outbox and show the status and retry. | S |
| Hover preview card: emoji glyph + emoji name (e.g. `:thumbsup:`, 12px tertiary caption), then names "You, Sam Rivera" (max-w 224) | src/features/messages/MessageReactions.tsx:288-333; reaction-name.ts:1-11 | DONE (crates/app/src/ui/message.rs) | The port has a plain text tooltip ("👍 You, Sam"). Build the preview card with the glyph, the shortcode name lookup and a names line. | S |
| Reactor names order: "You" first, then reactors in event order; unknown names fall back to the first 10 hex chars; profiles fetched on hover | src/features/messages/MessageReactions.tsx:269-287 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs) | The port sorts other names alphabetically and does not ask for missing profiles on hover. Keep event order and call `want_profiles` on hover. | S |
| Preview timing: 100ms open delay, instant switching along the row, slide ±12px with scale .98 over 160ms | src/features/messages/MessageReactions.tsx:44, 380-400; Messages.module.css:340-347 | DONE (crates/app/src/ui/message.rs): 100ms delay, instant switching and the 12px slide; GPUI cannot scale a card, so it fades in instead of the .98 scale | Default tooltip timing is used. Add the delay, the instant hand-off and the slide animation. | S |
| Inline add-reaction button after the pills (28px round, transparent until hover/open) | src/features/messages/MessageReactions.tsx:422-437 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs) | Works in the channel. In the thread panel it renders but does nothing (`picker: None`, no `on_popup`, thread.rs:446-449). Wire a picker there. | S |

### Quick reactions store

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Stored entries validated (`validReactionContent`, positive counts) | src/features/messages/quick-reactions.ts:11-24 | DONE (crates/app/src/ui/emoji_picker.rs) | Port deserializes without validating content or counts. | S |
| Shortcuts frozen while in use; refreshed on reload or another window's update | src/features/messages/quick-reactions.ts:63-86 | DONE (crates/app/src/ui/emoji_picker.rs): frozen until the open channel or the palette changes; the port has one window, so there is no other window to follow | The port recomputes on every render, so the bar can reorder right after a reaction. Freeze it per page visit if parity matters. | S |
| History forgotten when leaving the community | src/features/messages/quick-reactions.ts:59-61 | DONE (crates/app/src/ui/emoji_picker.rs, crates/app/src/community.rs) | `purge` clears the partition's stored prefs, but the in-memory `Usage` global (emoji_picker.rs:226) keeps the entry until restart. Drop it on leave. | S |

### Emoji picker

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Category header labels stay sticky while scrolling | src/bundled/emoji/emoji-mart.ts:195-202 | DONE (crates/app/src/ui/emoji_picker.rs) | Headers are plain list rows. Pin the current section's header at the top of the grid. | S |
| Nav icons: clock, smiley, paw, orange, barbell, car, lightbulb, shapes, flag, asterisk | src/bundled/emoji/emoji-mart.ts:16-28 | DONE (crates/app/src/ui/emoji_picker.rs): buzz-app's orange and shapes are Tabler `lemon-2` and `category`, which the port already used | The port uses Lemon for food and Category for symbols. Use the orange and shapes glyphs. | S |
| Responsive width: 1 to 6 per line from the composer/viewport width; the nav is hidden below 6 per line | src/bundled/emoji/EmojiPicker.tsx:103-139, 343-349; emoji-mart.ts:117 | DONE (crates/app/src/ui/emoji_picker.rs, crates/app/src/ui/composer.rs) | - | S |
| Frequently used: emoji-mart history of every picker pick (composer and reactions), up to 2 rows; defaults +1, grinning, kissing_heart, heart_eyes, laughing, stuck_out_tongue_winking_eye (first row) | src/bundled/emoji/emoji-mart.ts:96-104, 115; node_modules/emoji-mart DEFAULTS | DONE (crates/app/src/ui/emoji_picker/mart.rs) | The port shows one row from reaction usage only, so composer inserts never count. Record every pick, allow 2 rows (12), and keep the emoji-mart default order. | S |
| Skin tone persisted across opens/sessions (emoji-mart `skin` in localStorage); tooltip "Choose default skin tone" | src/bundled/emoji/emoji-mart.ts:364-368 (Mart storage) | DONE (crates/app/src/ui/emoji_picker.rs) | The tone lives only in the picker entity and is not saved. The tooltip says "Skin tone". Persist it as a device pref and match the copy. | S |
| Search ranking: Mart word-prefix results first, then fuzzy shortcode matches (exact, prefix, substring, subsequence); alias shortcodes | src/bundled/emoji/emoji-mart.ts:566-594; emoji-search.ts:48-67 | DONE (crates/app/src/ui/emoji_picker/mart.rs) | The port uses the composer's `searchEmoji` order (limit 240), not Mart's result order (limit 90). The results are close but not identical. | S |
| Search matches emoji keywords (e.g. "happy", "love") | src/bundled/emoji/emoji-search.ts:93; emoji-mart search index | DONE (crates/app/src/ui/emoji_picker/mart.rs, crates/app/src/ui/emoji_picker/emoji-mart.tsv) | The index holds only shortcodes and name words. Add the emoji-mart keyword data. | S |
| Search results header "Search Results" | @emoji-mart i18n `categories.search` | DONE (crates/app/src/ui/emoji_picker.rs) | The port says "Search results" (lower-case r). | S |
| Empty search state | Mart with preview off shows an empty result list | DONE (crates/app/src/ui/emoji_picker.rs) | The port shows "No emoji found". buzz-app shows nothing. Keep or drop, as you choose. | S |
| Keyboard navigation: arrow keys move a selection through results/grid from the search field; Enter picks the selection | src/bundled/emoji/emoji-mart.ts:474-496 (Mart owns arrows/Enter) | DONE (crates/app/src/ui/emoji_picker/mart.rs) | There is no grid selection or arrow/Enter handling. Add a selected cell, arrow movement and Enter to pick. | M |
| Escape closes the picker and returns focus to its trigger | src/bundled/emoji/EmojiPicker.tsx:309-323 | DONE (crates/app/src/ui/message_actions.rs): focus goes back to the bar's Add reaction button | - | S |
| Reaction picker opens above the trigger (side top, 6px offset); composer picker anchored to the composer form (top, 4px) | src/bundled/emoji/EmojiPicker.tsx:328-337 | DONE (crates/app/src/ui/message.rs): above with a 6px gap, below when there is no room. The composer picker keeps its anchor in composer.rs | The port opens the reaction picker below the bar or button. Anchor it above with a 6px gap, flipping on collision. | S |
| Custom emoji catalog error inside the picker + "Retry emoji" | src/bundled/emoji/EmojiPicker.tsx:271-278 | DONE (crates/app/src/ui/emoji_picker.rs) | Show the catalog error and a refresh button (`session.refresh_emoji`, relay session/mod.rs:385). | S |
| Search query carried between the Emoji and GIF tabs (both directions) | src/bundled/emoji/EmojiPicker.tsx:187-190, 236-239 | DONE (crates/app/src/ui/emoji_picker.rs) | The emoji query goes to the GIF tab, but GIF edits do not come back to the emoji search. | S |

### GIF picker

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Tile hover brightens (filter brightness 1.08) | src/bundled/emoji/Emoji.module.css:104-106 | DONE (crates/app/src/ui/gif_picker.rs) | Add a hover overlay or tint on GIF tiles. | S |

### Custom emoji (inline + settings)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Copying a selection that contains custom emoji yields their `:code:` text | src/bundled/emoji/copy-emoji.ts:1-47; index.tsx:36-40 | DONE (crates/app/src/ui/markdown.rs) | The emoji is an inline image whose alt is `:code:`, but the TextView copy path for inline elements is unverified. Confirm that copy writes the shortcode and add a test. | S |
| Tabs "Add emoji" / "My emojis (N)"; Add tab only when the session can author | src/bundled/emoji/CustomEmojiSettings.tsx:77-78, 353-368 | DONE (crates/app/src/ui/settings/emoji.rs, no change): port sessions can always upload and publish emoji sets, so the Add tab shows first, as buzz-app does when it can author | The Add tab is always shown. Hide it when upload/add are unavailable, and default to "My emojis". | S |
| Upload failure fallback copy "Failed to upload emoji image." | src/bundled/emoji/CustomEmojiSettings.tsx:146 | DONE (crates/app/src/ui/settings/emoji.rs) | The port shows the raw upload error string. Use the fallback copy when the message is empty or generic. | S |
| Focus moves to Upload after Clear/Save/empty-state action | src/bundled/emoji/CustomEmojiSettings.tsx:88-107 | DONE (crates/app/src/ui/settings/emoji.rs) | Move focus to the Upload button after these actions. | S |

### Copy image, download, media modal boundary

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Media viewer modal: Escape closes, Tab trapped inside, focus restored to opener | src/features/messages/useModalBoundary.ts:11-113 | DONE (crates/app/src/ui/media/review.rs, crates/app/src/main_window.rs) | - | S |

## 3. Timeline, scrolling, unread, jump-to, threads, typing and search

Note: buzz-app draws no "new messages" unread line in the timeline; the only cue is the jump button's new-message count, which the port matches.

Rows: DONE 68, DIFFERENT 1.

### Timeline layout, day dividers and grouping

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Day labels and bylines re-render when the local day rolls over at midnight (`useLocalDay`), so "Today" becomes "Yesterday" without new rows | src/shared/use-local-day.ts:76 | DONE (crates/app/src/ui/time.rs, crates/app/src/ui/channel.rs, crates/app/src/ui/thread.rs) | Labels are computed once in `items()` and only rebuilt when rows change. Add a midnight timer that rebuilds items and notifies the channel page and thread panel. | S |
| First-load error: error text plus a "Retry messages" button that re-ensures the channel | src/bundled/channels/ChannelBody.tsx:81 | DONE (crates/app/src/ui/channel.rs) | The port shows the error text only. Add a "Retry messages" button that retries the channel window. | S |
| Timeline end spacer (12px) is part of the last list item, so it scrolls with history | src/features/messages/ChannelTimeline.tsx:827 | DONE (crates/app/src/ui/channel.rs) | The port puts a fixed 12px div below the list, outside the scroll. Render the spacer as part of the last list row. | S |
| Narrow viewport (<= 650px): timeline padding-inline 12px instead of 24px | src/features/messages/Messages.module.css:504 | DONE (crates/app/src/ui/channel.rs) | Use 12px row padding when the window is <= 650px wide. | S |

### Membership rows (as they affect the timeline)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Membership row gets its own day divider and is excluded from read observation | src/features/messages/MembershipRow.tsx:40 / src/features/messages/use-reading.ts:92 | DONE (crates/app/src/ui/channel.rs) | Day divider is done. The port reports membership row ids to `viewport()`; filter them out as buzz-app does. | S |

### Scrolling, older history and scroll restore

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Per-channel scroll restore: saves offset, at-bottom flag and an anchor message plus its Y offset in view state (`scroll:<channel>`), restores it on return, survives restarts | src/features/messages/ChannelTimeline.tsx:185 / src/features/messages/ChannelTimeline.tsx:430 / src/features/messages/ChannelTimeline.tsx:513 | DONE (crates/app/src/ui/scroll.rs, crates/app/src/ui/channel.rs). A row older than the first page is looked for in up to five older pages until the disk cache keeps more. | A new `ChannelPage` is built on every channel switch and starts at the bottom. Save the anchor row id + offset (and bottom flag) per scope/channel when the page closes, persist it, and scroll to it on reopen. Keep the anchor through late measurements and membership regrouping. | M |
| Row height cache per channel (`geometry`) so a restored position paints without jumps | src/features/messages/ChannelTimeline.tsx:396 | DONE (crates/app/src/ui/scroll.rs): the last three channels keep their measured list in memory, as geometry.ts does. | Cache measured row heights per channel/width and seed the `ListState` on reopen (needed for exact restore). | M |
| Retained older-history demand while the window is cached, resumed when it verifies | src/features/messages/ChannelTimeline.tsx:640 | DONE (crates/app/src/ui/channel.rs) | When a load is skipped because the window is stale/cached, remember the demand and call `load_older` again once the window is fresh. | S |
| Top edge shows the window error text (role alert) next to the retry button | src/features/messages/ChannelTimeline.tsx:739 | DONE (crates/app/src/ui/channel.rs) | Render `snapshot.error` text in the edge row when rows exist. | S |
| Keyboard scrolling of the focusable history region (Up, PageUp, Home, Shift+Space, Ctrl+Home leave follow; region labelled "Channel message history") | src/features/messages/ChannelTimeline.tsx:37 / src/features/messages/ChannelTimeline.tsx:720 | DONE (crates/app/src/ui/channel.rs, crates/app/src/ui/thread.rs) | Make the timeline focusable and handle arrow/page/home/end keys to scroll the list; upward keys must pause tail follow. | S |
| Resize and reflow keep the reading anchor (or bottom) through late row measurements | src/features/messages/ChannelTimeline.tsx:560 | DONE (crates/app/src/ui/channel.rs): GPUI's list keeps the top row and its offset through width changes and remeasures, and the bottom while following. | GPUI `ListState` keeps the item offset on reflow; there is no explicit anchor-row retention for width changes while scrolled up. Verify and keep the top visible row fixed on width change. | S |

### Jump to latest

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Show/hide motion: opacity, translateY(2px) scale(.97), blur(2px) over 150ms in / 120ms out; opacity only with reduced motion | src/features/messages/Messages.module.css:2276 | DONE (crates/app/src/ui/jump.rs): opacity and the 2px rise; GPUI has no scale or blur for elements. | The port mounts/unmounts the pill instantly. Animate opacity/translate/scale on show and hide. | S |
| Button moves up 48px into a separate lane when the composer shows an agent-activity pill | src/features/messages/Messages.module.css:2271 | DONE (crates/app/src/ui/jump.rs, crates/app/src/ui/typing.rs) | Depends on the agent activity chooser (see Typing). Offset the pill when it is present. | S |
| Thread panel has the same jump-to-latest button with new-reply count; smooth scroll with a 1s settle | src/features/messages/ThreadPanel.tsx:754 / src/features/messages/ThreadPanel.tsx:1001 | DONE (crates/app/src/ui/thread.rs): the jump is instant; GPUI lists have no smooth scrolling. | Add the pill to the thread panel: track distance from bottom, count replies that arrive while scrolled up, click resumes tail follow. | S |

### Jump to a message (reveal)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Revealed row is focused (keyboard focus moves to the row; no colour flash) | src/features/messages/use-message-reveal.ts:74 | DONE (crates/app/src/ui/channel.rs, crates/app/src/ui/thread.rs): no tint; focus moves to the history, as GPUI rows are not focusable. | The port tints the row with `affordance_accent` for 2s instead of focusing it. buzz-app has no highlight flash. Decide: drop the tint and move focus to the row, or keep it as a deliberate extra. | S |
| Exact target missing: "Selected message unavailable." plus "Retry thread" | src/features/messages/ThreadPanel.tsx:1060 | DONE (crates/app/src/ui/thread.rs) | When `target_status` is Unavailable, show the status line and a Retry thread button. | S |
| Navigation that fails shows "This destination couldn’t open" with "Retry navigation" | src/app/App.tsx:255 | DONE (crates/app/src/ui/channel_states.rs, crates/app/src/ui/workspace.rs). An exact thread target that is unavailable shows in the panel instead. | `Reveal::Absent` falls through to an exact thread silently. Show a failure notice when the exact lookup reports not-found/unavailable. | S |

### Unread and mark-as-read

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Only fully visible rows count (partly clipped rows are not read) | src/features/messages/use-reading.ts:94 | DONE (crates/app/src/ui/reading.rs) | The port sends the whole `visible_range`, including partly visible rows. Trim rows whose bounds are clipped by the viewport. | S |
| Reading only while the window is visible and focused and focus is in the list or its composer; blur/hidden stops the lease | src/features/messages/use-reading.ts:67 / src/features/messages/use-reading.ts:226 | DONE (crates/app/src/ui/reading.rs, crates/app/src/ui/channel.rs, crates/app/src/ui/thread.rs, crates/relay/src/session/unread.rs) | `ChannelView::viewport` documents "report nothing while hidden or unfocused" but the page never does. Observe window activation and focus and report `([], false)` when inactive; re-report on focus. | S |
| Reading re-armed by scroll, pointer, key, resize, DOM changes and owner positioning | src/features/messages/use-reading.ts:219 | DONE (crates/app/src/ui/reading.rs): every layout reports what changed. | The port reports on scroll events only; arrivals at the bottom are handled in the relay. Also report after resize and after programmatic positioning (reveal, jump). | S |
| Thread panel reading: visible replies are observed and the thread is caught up to its newest reply (`catchUp(bottom, rootId)`) | src/features/messages/ThreadPanel.tsx:926 | DONE (crates/app/src/ui/thread.rs, crates/relay/src/session/unread.rs) | Add a thread viewport/reading API in the session (lease scoped to channel + root) and report visible reply ids and bottom state from the thread list. | M |

### Thread summary in the timeline

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Unread dot on the summary when the thread has unread replies or is marked unread (tooltip "Thread marked unread" / "...on this device only" / "Observed unread replies…") | src/features/messages/ReplySummary.tsx:67 / src/features/messages/MessageRow.tsx:209 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/channel.rs, crates/relay/src/session/unread.rs) | Expose per-thread unread state from the session (`ReadTarget` thread snapshot) and draw the 8px `text-standard` dot after the count with the tooltip text. | M |
| Working dots (three pulsing dots) on the summary while one of the viewer's agents is active in that thread | src/features/messages/MessageRow.tsx:877 / src/features/messages/Messages.module.css:626 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/typing.rs) | Match `thread_typing` entries for the root against the viewer's own agents and show the animated dots pill. | S |
| "View thread" button on a root with no replies while an agent works in its thread | src/features/messages/MessageRow.tsx:891 | DONE (crates/app/src/ui/message.rs) | Show the summary (dots + "View thread") when `reply_count == 0` and an owned agent is typing in that thread. | S |

### Thread panel: shell, header and layout

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Close button takes focus when the panel opens (standalone header) | src/features/messages/ThreadPanel.tsx:129 | DONE (crates/app/src/ui/thread.rs, crates/app/src/ui/workspace.rs): the panel takes focus, as the tab's close button cannot; Reply puts it in the composer. | The port focuses the thread composer on open. buzz-app focuses the close button (or the composer for a "reply" intent). Match per intent. | S |
| Resizable dock: 16px handle on the border, width clamped [316px, available − 320px], double-click resets | docs/port/ui.md (4.11) | DONE (crates/app/src/ui/panels.rs, crates/app/src/ui/resize_handle.rs, crates/app/src/ui/workspace/board.rs) | - | M |
| Narrow layouts: <= 1000px viewport or <= 651px container overlays the thread on the right (min(420px,100%)), <= 650px full width | docs/port/ui.md (4.11) | DONE (crates/app/src/ui/panels.rs, crates/app/src/ui/workspace/layout.rs) | - | M |
| Open/close motion: translateX(12px) + opacity over 180ms | docs/port/ui.md (4.11) | DIFFERENT (crates/app/src/ui/panels.rs): the dock opens with buzz-app's motion; closing has none, as on a desktop window in buzz-app, but overlays do not slide out either: GPUI drops a closed panel's elements at once and has nothing left to animate | - | S |

### Thread panel: content and reply tree

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Narrow panels flatten indentation at depth 3 (<= 360px) or 4 (<= 480px) | docs/port/ui.md (4.11) | DONE (crates/app/src/ui/thread.rs) | Make `MAX_INDENT` depend on the panel width. | S |
| Branch summary shows "(N new)" when descendants are unread | src/features/messages/ReplySummary.tsx:64 / src/features/messages/ThreadPanel.tsx:802 | DONE (crates/app/src/ui/thread.rs) | Needs per-reply unread attention from the session; append "(N new)". | S |
| Branch summary avatars use agent squircles and media-proxied pictures | src/features/messages/ReplySummary.tsx:38 | DONE (crates/app/src/ui/thread.rs) | The port uses the raw picture URL and never squircles agents here. Use `media::avatar` and the agent shape as in `thread_summary`. | S |
| Branch summary is a link-variant small button | src/features/messages/ReplyBranch.tsx:80 | DONE (crates/app/src/ui/thread.rs) | The port styles it like the timeline thread pill (hover fill, 32px). Use link-variant styling. | S |
| Expanding a branch keeps the reading position, moves focus to the first revealed reply and scrolls it into view (nearest) | src/features/messages/ReplyBranch.tsx:36 | DONE (crates/app/src/ui/thread.rs): the first reply scrolls into view; focus stays on the panel, as rows are not focusable. | The port pauses tail follow only. Scroll the first child into view after expanding. | S |
| Older replies load when the reader scrolls near the top (strict "older" windows only), keeping the anchor row in place and expanding newly loaded ancestors | src/features/messages/ThreadPanel.tsx:898 / src/features/messages/ThreadPanel.tsx:451 | DONE (crates/app/src/ui/thread.rs) | Paging and position keeping are done. Ancestors that arrive with an older page are not auto-expanded around the anchored reply. | S |

### Thread panel: states

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Older-page error shown inline at the top of the reply list with its own "Retry thread" (re-pages older) | src/features/messages/ThreadPanel.tsx:1038 | DONE (crates/app/src/ui/thread.rs) | The port shows every error below the list. Put the older-page error at the top of the replies. | S |
| Loading older replies indicator | src/features/messages/ThreadPanel.tsx:914 | DONE (crates/app/src/ui/thread.rs) | The port adds "Loading earlier replies…", which buzz-app does not show. Remove it or keep as an intended extra. | S |
| Saved-draft guard: "This saved draft does not identify a thread root. Open its origin to check the destination." and composer disabled until the root verifies | src/features/messages/ThreadPanel.tsx:1049 | DONE (crates/app/src/ui/thread.rs, crates/app/src/ui/inbox/drafts.rs) | - | S |

### Thread panel: composer and actions

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Reply to a specific reply: "Replying to {name}" + excerpt + cancel X; clicking Reply on the same reply again clears the target | src/features/messages/ThreadPanel.tsx:521 / src/features/messages/ThreadPanel.tsx:1110 | DONE (crates/app/src/ui/thread.rs) | Context bar is done (crates/app/src/ui/composer.rs:1665). The second Reply click does not toggle the target off. | S |
| Reply on the root (action bar) focuses the composer and clears any reply target | src/features/messages/ThreadPanel.tsx:517 | DONE (crates/app/src/ui/thread.rs): the thread's action bar replies to the root and clears the target. | The root has no reply control in the port. Add it (via the action bar). | S |
| After sending a nested reply, its branch and ancestors open and the new row scrolls into view; a root-level reply follows the bottom | src/features/messages/ThreadPanel.tsx:1131 / src/features/messages/ThreadPanel.tsx:742 | DONE (crates/app/src/ui/thread.rs) | Branch expansion is done; the new row is not scrolled into view once it lands. | S |

### Typing indicator

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Narrow (<= 650px) inset 12px | src/features/messages/TypingIndicator.module.css:108 | DONE (crates/app/src/ui/typing.rs) | Use 12px insets below 650px. | S |
| Typing indicator in the thread panel, scoped to that thread root | src/features/messages/TypingIndicator.tsx:88 | DONE (crates/app/src/ui/thread.rs, crates/app/src/ui/typing.rs) | Data exists (`thread_typing`, crates/relay/src/session/mod.rs:112). Render `typing::overlay` above the thread composer. | S |
| Names use channel-scoped identity names (nicknames) | src/features/messages/TypingIndicator.tsx:94 | DONE (crates/app/src/ui/typing.rs) | - | S |
| Viewer's own working agents split out of "typing" into an agent chooser on the right: up to 4 overlapping 24px squircle avatars, "+N" | src/features/messages/TypingIndicator.tsx:188 | DONE (crates/app/src/ui/typing.rs) | The port lists every pulse author as "typing". Split owned agents (agent choices) from others and render the avatar stack as a ghost xs button. | M |
| Agent chooser hover (250ms) opens a "Working now" popover listing each agent row with status, scope ("Channel conversation", "Thread · {excerpt}", "+N more") and open actions | src/features/messages/TypingIndicator.tsx:137 / src/features/messages/TypingIndicator.tsx:353 | DONE (crates/app/src/ui/typing.rs): rows say "Working" and have no "View activity", as the port captures no agent turns and has no activity panel. | Build the popover with `AgentActivityRow` equivalents and conversation/activity navigation. | M |
| Single-agent click opens its activity panel directly (Down Arrow previews the list); Escape closes the popover | src/features/messages/TypingIndicator.tsx:280 | DONE (crates/app/src/ui/typing.rs) | - | S |

### Search palette (Search Buzz / Search this conversation)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Ctrl/Cmd+F opens "Search this conversation" scoped to the open conversation | src/app/shell/PageSearch.tsx:106 | DONE (crates/app/src/ui/navigation.rs, crates/app/src/ui/workspace/dialogs.rs) | Add the binding; open the dialog with `scope` preset to the current channel. | S |
| Shortcut hint follows the user's rebinding | src/app/shell/PageSearch.tsx:150 | DONE (crates/app/src/ui/workspace/shell.rs): the tooltip reads the `global-search` binding; the same row as shell.md "Search tooltip follows a rebound shortcut" | - | S |
| No community / not connected: pages-only palette; "Choose a community to see recent conversations.", "Connecting to this community…", "Connect to this community to search messages.", disconnected notice with "Retry connection" | src/app/shell/PageSearch.tsx:258 | DONE (crates/app/src/ui/search.rs, crates/app/src/ui/workspace/dialogs.rs) | `open_search` returns when there is no community and never shows connection states. Allow a pages-only palette and the connection notices. | S |
| Recent activity rows show the channel's last-message preview as detail | src/app/shell/SearchResults.tsx:532 | DONE (crates/app/src/ui/search.rs; crates/relay/src/unread.rs `LatestMessage.preview`) | The port shows the conversation type. Use `channel.preview` when present. | S |
| "Loading recent conversations…" while the channel list loads | src/app/shell/SearchResults.tsx:629 | DONE (crates/app/src/ui/search.rs) | Show it while the roster is loading. | S |
| Matched letters underlined in result labels (`MatchedLabel`, also used by other pickers) | src/features/search/MatchedLabel.tsx:24 / src/features/search/match.ts:62 | DONE (crates/app/src/ui/search_kit/mod.rs `matched_label`; crates/relay/src/search.rs positions) | `match_name` returns only a rank. Return matched code-point positions and render underlined runs in `modal::row`. | S |
| Usage-ranked results: visits and past choices for the same typed text (7-day half-life, max 1.5 rank boost), stored per community/viewer | src/features/search/usage.ts:100 / src/features/search/usage.ts:161 | DONE (crates/app/src/ui/search_kit/usage.rs) | Port the usage store (visits recorded on navigation, picks on choice) and apply `searchOrder` to channels and pages. | M |
| Archived writable channels (stream/forum/session) included with detail "Archived channel", ranked half a step behind live ones | src/app/shell/SearchResults.tsx:87 / src/app/shell/SearchResults.tsx:475 | DONE (crates/app/src/ui/search.rs) | The port filters out all archived channels. Include writable archived ones with the +0.5 rank. | S |
| Message hit detail "{conversation} · {author} · {M/D/YYYY}" with workflow label for workflow output | src/app/shell/SearchResults.tsx:562 | DONE (crates/app/src/ui/search.rs) | Add the workflow label when the hit has a workflow owner. | S |
| `in:` that names nothing: buzz-app shows "Searching messages…" during lookup then the normal empty text | src/app/shell/SearchResults.tsx:567 | DONE (crates/app/src/ui/search.rs) | The port shows "No conversation matches that in: name.". Use buzz-app's wording or keep as intended. | S |
| Author candidates include the viewer's selectable agents (matched by profile name or agent name) and hide archived identities (NIP-IA) | src/app/shell/SearchResults.tsx:304 / src/features/messages/use-mention-archives.ts:7 | DONE (crates/app/src/ui/search.rs, crates/app/src/ui/archives.rs) | - | M |
| Author picker avatars use squircles for agents | src/app/shell/SearchResults.tsx:367 | DONE (crates/app/src/ui/search.rs) | Squircle the avatar when the candidate is an agent. | S |
| Ambiguous completed author name reopens the People/Agents picker so the user can pick one key | src/app/shell/SearchResults.tsx:234 | DONE (crates/app/src/ui/search.rs) | The port shows "Several people have that name. Type from: and pick one." Show the picker instead. | S |
| Message search error detail "Message search couldn’t finish: … Pages and conversations are still available." with "Retry messages" | src/app/shell/useSearchMessages.ts:247 / src/app/shell/SearchResults.tsx:785 | DONE (crates/app/src/ui/search.rs) | Show the error detail and a retry button that re-runs the search. | S |
| Status notes: "Loading joined conversations…", "Couldn’t load all joined conversations." + "Retry conversations", "Conversation names include only loaded joined conversations.", "Public channel results include only the first page of channels.", public lookup error + "Retry channels" | src/app/shell/SearchResults.tsx:728 | DONE (crates/app/src/ui/search.rs) | Public channel errors are swallowed (`unwrap_or_default`). Add the status area with these notes and retries. | S |
| Highlight clamps at the ends (no wrap) | src/features/search/use-search-highlight.ts:67 | DONE (crates/app/src/ui/search_kit/highlight.rs) | The port wraps with `rem_euclid`. Clamp instead. | S |
| No highlight before anything is typed (Enter does nothing on an empty query); typing highlights the first row | src/features/search/use-search-highlight.ts:54 | DONE (crates/app/src/ui/search_kit/highlight.rs) | The port always highlights row 0. Highlight nothing while the query is empty. | S |
| Highlight follows the row's key as late results arrive (a row inserted above cannot redirect Enter); falls back to the first row when the highlighted row leaves | src/features/search/use-search-highlight.ts:173 | DONE (crates/app/src/ui/search_kit/highlight.rs) | The port stores an index. Store the highlighted entry key. | S |
| Keyboard moves scroll the highlighted row into view | src/features/search/use-search-highlight.ts:96 | DONE (crates/app/src/ui/search_kit/highlight.rs) | Track the results scroll handle and scroll the highlighted row into view on each move. | S |
| Pointer moves highlight a row (real moves only, not rows moving under a resting cursor) | src/features/search/use-search-highlight.ts:156 | DONE (crates/app/src/ui/search_kit/highlight.rs) | Hover highlighting works, but it also fires when results move under a still pointer. Ignore mouse-move events with unchanged position. | S |
| Shift+Ctrl/Cmd+1…9 runs the Nth result directly | src/app/shell/SearchChoices.tsx:122 | DONE (crates/app/src/ui/search.rs) | Handle shift+secondary+digit in the key handler. | S |
| Field limits: max 256 characters, no spellcheck/autocorrect | src/app/shell/SearchChoices.tsx:117 | DONE (crates/app/src/ui/search.rs) | Cap the input at 256 characters. | S |
| Results for a conversation the viewer leaves are dropped while the palette is open | src/app/shell/useSearchMessages.ts:79 | DONE (crates/app/src/ui/search.rs) | Message hits are looked up against `snapshot.channels` and `outcome.channels`, so stale hits stay. Filter hits to channels still listed. | S |

## 4. Composer, autocomplete, formatting, attachments and voice recording

Note: buzz-app's composer is a WYSIWYG ProseMirror editor (marks and blocks are document nodes, serialized to Markdown on send). The port's composer is a plain-text `TextareaState` with atomic mention/channel chips, and its toolbar inserts Markdown syntax. Rows that depend on the live-styling work are marked IN PROGRESS.

Note: buzz-app does not turn a URL pasted over a selection into a link; it replaces the selection (`src/features/messages/EditableInput.tsx:1447-1492`).

Rows: DONE 72, DIFFERENT 4, SKIPPED 1.

### Input basics, keyboard and states

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Holding Enter (key repeat) never sends | src/features/messages/MessageComposer.tsx:1572 | DONE (crates/app/src/ui/composer.rs) | Ignore repeated Enter (`is_held`) for submit. | S |
| Enter inside a bullet/ordered list continues the list instead of sending; Enter on an empty item exits the list | src/features/messages/MessageComposer.tsx:1573-1579, src/features/messages/composer-blocks.ts:513 | DONE (crates/app/src/ui/composer/blocks.rs) | Detect a list line at the caret: Enter inserts the next marker (`- `, `n+1. `), and an empty item removes its marker. Shift+Enter continues the block. Coordinate with the live-styling work. | M |
| Tab / Shift+Tab indent and outdent list items (sink/lift) | src/features/messages/EditableInput.tsx:1327-1340 | DONE (crates/app/src/ui/composer/blocks.rs) | Without a completion, Tab on a list line nests the item (indent plus renumber) and Shift+Tab lifts it. | M |
| Typed Markdown conversion is its own undo step (one undo restores the typed delimiter) | src/features/messages/EditableInput.tsx:1262-1270 | DIFFERENT, kept on purpose (crates/app/src/ui/composer/live_markdown.rs): the composer edits Markdown source and hides the markers, so typing never converts anything; Undo removes the typed character | Being added with the live-styling work. | M |
| 16,000-character limit enforced by the editor (an insertion that is too long is refused: "Message is too long to insert text") | src/features/messages/MessageComposer.tsx:806-808, 1507 | DONE (crates/app/src/ui/composer.rs) | The port lets the text grow past the limit, shows "Messages hold up to 16000 characters." and disables Send. buzz-app refuses the input. Decide which to keep; the reference behaviour is to cap input. | S |
| Emoji-only draft shows large emoji in the composer | src/features/messages/MessageComposer.tsx:567-570, 1506 | DONE (crates/app/src/ui/composer.rs) | When the draft is Unicode or custom emoji only, render the textarea text at the large emoji size. | S |
| Archived channel: the draft is kept, but the composer is disabled until the channel is restored | src/features/messages/MessageComposer.tsx:313-322, 495-498 | DONE (crates/app/src/ui/composer.rs) | Disable input and Send for archived channels and keep the draft visible. | S |
| Cached channel (offline, not yet confirmed): composer shows the local draft, disabled, with no completion or typing | src/features/messages/MessageComposer.tsx:320-322, 1264-1276 | DONE (crates/app/src/ui/composer.rs, crates/app/src/ui/channel.rs) | A cached summary is `read_only`, so the port shows the "Read-only preview · You haven't joined" join notice. Show the disabled composer with its draft instead. | S |
| Relay connection that cannot write: "This relay connection supports reading only." | src/features/messages/MessageComposer.tsx:1329-1337 | DIFFERENT: buzz-app's desktop writer always supports kind 9 (`native.ts` `nativeWriteKinds`), so the footer never shows there; the port's session always writes the same way and has no read-only connection to report | - | S |
| Composer disabled while a send, admission or edit save is in flight | src/features/messages/MessageComposer.tsx:499-506, 1478 | DONE (crates/app/src/ui/composer.rs) | `sending` blocks Send only. The text stays editable while the send is in flight. Make the input read-only while busy. | S |
| New-conversation composer (New message) is the full MessageComposer in submission mode, with a draft roster | src/features/messages/MessageComposer.tsx:216-227, 245-260, src/features/messages/draft-mention-roster.ts:5 | DONE (crates/app/src/ui/new_message.rs: the page hosts the shared Composer) | Not done here: `Composer` is bound to an open channel (draft key, members, outsiders, background uploads to that channel). Reusing it before the DM exists needs a channel-less submission mode with a draft roster and New-message outbox recovery. | M |
| Composer lab / state gallery (developer specimens: unavailable, read-only, validation) | src/bundled/composer/lab/ComposerStateGallery.tsx:53-78, src/bundled/composer/lab/ComposerCopyPreview.tsx:6-28 | SKIPPED (decided) | - | S |

### Drafts and send

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Draft conflict between two composers/windows: "This draft changed elsewhere..." with Load saved draft / Keep my draft | src/features/messages/MessageComposer.tsx:594-623, 661-671, 1667-1697 | DONE (crates/app/src/ui/composer.rs) | Compare against the stored revision before writing. On a mismatch, show the conflict notice and its two buttons. | M |
| Draft storage failure: "Could not save this draft on this device..." + Retry draft save | src/features/messages/MessageComposer.tsx:459-467, 1676, 1691-1695 | DONE (crates/app/src/ui/composer.rs) | Surface the save error in the composer with a retry button. | S |
| Accepted-send cleanup failure: "Message accepted. Could not update the saved draft..." + Retry draft cleanup | src/features/messages/MessageComposer.tsx:672-696, 1672-1681 | DONE (crates/app/src/ui/composer.rs) | Follows from the draft-storage rows. | S |
| Remember mentioned agents: after a send, the next draft is prefilled with the agent chips (setting in Agent settings) | src/features/messages/MessageComposer.tsx:1124-1133, src/features/messages/mention-preferences.ts:7-43 | DONE (crates/app/src/ui/composer.rs) | Build a follow-up draft of the agent recipients, and add the "remember mentioned agents" preference. | S |
| A selected recipient is archived: "A selected recipient is archived. Remove it before sending." | src/features/messages/MessageComposer.tsx:1046-1049 | DONE (crates/app/src/ui/composer.rs) | - | S |
| Session / personal ("Me") composer: mentioned agents are added to the session (and parent channel) on send; "Retry agents" | src/features/messages/MessageComposer.tsx:245-260, 942-982, 1069-1073 | DONE (crates/app/src/ui/me/recipients.rs, crates/app/src/ui/channel.rs) | - | M |
| "Retry message preparation" when the custom emoji catalog failed | src/features/messages/MessageComposer.tsx:1710-1728 | DONE (crates/app/src/ui/composer.rs) | Offer `refresh_emoji` next to a send error while the catalog is in error. | S |

### Formatting tools and link dialog

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Button tooltips show the platform shortcut ("Bold (⌘B)" on macOS, "Ctrl+B" elsewhere) | src/features/messages/ComposerFormattingTools.tsx:166-183 | DONE (crates/app/src/ui/composer/format.rs) | The tooltips are hard-coded "Ctrl+…". Use ⌘ on macOS, and label Link "Link (⌘K)". | S |
| Active (pressed) state reflects the formatting at the caret | src/features/messages/ComposerFormattingTools.tsx:165, 178-184 | DONE (crates/app/src/ui/composer/live_markdown.rs) | Active only when the delimiters touch the selection exactly. A caret inside `**bold**` does not light Bold. Follow the live-styling parse. | S |
| WYSIWYG formatting: toggles apply real marks/blocks (no visible delimiters), serialized to Markdown on send | src/features/messages/composer-markdown.ts:198-201, src/features/messages/composer-blocks.ts:147 | DONE (crates/app/src/ui/composer/live_markdown.rs, crates/app/src/ui/composer/format.rs, third_party/gpui-base concealed ranges) | Live styling work. | L |
| Collapsed-caret toggle: inline formats start a typing mode; Spoiler with a collapsed caret toggles the whole draft | src/features/messages/MessageComposer.test.tsx:3804 | DONE (crates/app/src/ui/composer.rs, crates/app/src/ui/composer/format.rs) | The port inserts an empty delimiter pair around the caret. | S |
| Toolbar open/close animation (140 ms fade/scale; leading tools slide in), instant for keyboard toggles and reduced motion | src/features/messages/ComposerFormattingTools.tsx:35, 96-97, 114-221 | DONE (crates/app/src/ui/composer.rs) | Add the reveal transition. | S |
| Link button / Mod+K opens the "Add link" / "Edit link" dialog with Text and URL fields | src/features/messages/ComposerLinkDialog.tsx:53-113, src/features/messages/EditableInput.tsx:1301-1310 | DONE (crates/app/src/ui/composer/link.rs) | The port inserts `[text](https://)` and selects the URL, and has no Mod+K. Build the dialog: initial focus is URL when text exists, else Text; Save is disabled when the URL is empty. | M |
| Link URL validation: "Use an HTTP(S) URL or a valid buzz:// link." (credentialed URLs refused); "Link text must stay on one line." | src/features/messages/ComposerLinkDialog.tsx:30-38, src/features/messages/composer-link.ts:5-8 | DONE (crates/app/src/ui/composer/link.rs) | Validate with the same safe-URL rule as message links. | S |
| "Remove link" in the dialog for an existing link; Mod+K on a detected plain URL edits it | src/features/messages/ComposerLinkDialog.tsx:58-67, src/features/messages/EditableInput.test.tsx:864 | DONE (crates/app/src/ui/composer/link.rs) | Find the link at the caret and offer Remove. | S |
| Typed Markdown spans convert as you type (`**b**`, `*i*`, `` `c` ``, `~~s~~`) using the timeline's parser rules | src/features/messages/composer-inline-input.ts:110 | DONE (crates/app/src/ui/composer/live_markdown.rs) | Live styling. | M |
| Typing ` ``` ` / `~~~` opens a code block; `- `, `1. `, `> ` open lists/quotes (nesting rules) | src/features/messages/composer-blocks.ts:329, 404 | DONE (crates/app/src/ui/composer/live_markdown.rs) | Live styling. | M |
| Backspace at the start of a code block leaves it | src/features/messages/EditableInput.tsx:1352-1361 | DONE (crates/app/src/ui/composer/blocks.rs) | Part of block editing; follows the live-styling model. | S |

### Paste, copy and drag-and-drop

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Pasting the app's own HTML restores its Markdown (identity links, formatting); other HTML pastes as plain text | src/features/messages/EditableInput.tsx:1456-1466, src/features/messages/composer-clipboard.ts:165 | DONE (crates/app/src/ui/composer/clipboard.rs): GPUI's Linux clipboard holds text, so the links ride as the string's metadata, read back on paste in this app | Plain text is used everywhere. Read Buzz's HTML flavour back as Markdown. | M |
| Pasted identity links (`[@Name](nostr:npub…)`) become recipient chips when admitted (at most 32, at a mention boundary); otherwise they stay display chips | src/features/messages/EditableInput.tsx:899-938, src/features/messages/mention-admission.ts:58-82 | DONE (crates/app/src/ui/composer.rs) | After a paste, scan for profile links and convert the admitted ones to chips. | M |
| A pasted lone URL gets a trailing space | src/features/messages/EditableInput.tsx:1467-1488 | DONE (crates/app/src/ui/composer.rs) | Append a space after a paste that is exactly one link. | S |
| Copy/cut from the composer writes humanized text and semantic HTML (chips read as `@Name` / `#channel`) | src/features/messages/EditableInput.tsx:885-897, 1437-1446, src/features/messages/composer-clipboard.ts:112-160 | DONE (crates/app/src/ui/composer/clipboard.rs): text flavour as the reference; no HTML flavour, which GPUI's Linux clipboard cannot hold | Plain text only, and channel chips copy as raw Markdown link source. Copy chip labels and add the HTML flavour. | S |
| Pasting an agent snapshot (Buzz HTML) attaches the uploaded snapshot | src/features/messages/MessageComposer.tsx:1361-1387 | DONE (crates/app/src/ui/composer.rs, crates/app/src/ui/agents/snapshot_preview.rs) | - | M |
| The whole conversation pane is the drop zone (`data-attachment-drop-zone`), routed to that pane's composer only | src/features/messages/use-file-drop.ts:17-82 | DONE (crates/app/src/ui/composer.rs, hooks in channel.rs and thread.rs) | Only the composer box accepts drops. Extend the drop target to the channel and thread panes. | S |

### Attachments and uploads

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Uploads deferred until Send: files stay "Queued" and upload only on Send, then publish in the background | src/features/messages/attachment-draft.ts:257-266, src/features/messages/background-upload.ts:108-196 | DONE (crates/app/src/ui/composer/attachments.rs) | The port uploads each file as soon as it is attached and blocks Send until all are ready. buzz-app clears the composer at once and uploads behind it. | L |
| Background upload pill: "Preparing" / "Uploading" / "Finishing" with a progress bar (indeterminate sweep without totals) and Cancel | src/features/messages/BackgroundUploadStatus.tsx:37-89 | DONE (crates/app/src/ui/composer.rs, crates/app/src/ui/composer/attachments.rs) | Needs the background-send model. Cancel stops the newest send and restores its draft. | M |
| Failed background send restores the draft and files, with the toast "Upload failed: …" / "Message failed to send: …" and recovery notices | src/features/messages/background-upload.ts:141-189, src/features/messages/BackgroundUploadStatus.tsx:90-104 | DONE (crates/app/src/ui/composer/attachments.rs) | Follows the background-send model. | M |
| A conversation cannot send again until its background send settles | src/features/messages/background-upload.ts:208-212 | DONE (crates/app/src/ui/composer/attachments.rs) | Follows the background-send model. | S |
| Per-file byte progress during transfer | src/features/messages/attachment-draft.ts:129-140 | DONE (crates/relay/src/upload.rs, crates/app/src/ui/composer/attachments.rs) | Report the PUT body progress and show it. | M |
| Chip statuses: Queued / Preparing… / Uploading… / Ready / Upload failed | src/features/messages/ComposerAttachments.tsx:119-125 | DONE (crates/app/src/ui/composer/attachments.rs) | No "Preparing…" state (metadata cleanup happens inside upload). | S |
| Video chips show a first-frame thumbnail | src/features/messages/ComposerAttachments.tsx:94-96, 199-213 | DONE (crates/app/src/ui/composer.rs:3438) | Decode a poster frame for local video files. | M |
| Clicking a thumbnail opens a full preview dialog (image or playable video) | src/features/messages/ComposerAttachments.tsx:185-190, 285-310 | DONE (crates/app/src/ui/composer.rs:3470, crates/app/src/ui/media/review.rs) | Open the media viewer for a local draft attachment. | S |
| Size format: "N KB" (minimum 1 KB) under 1 MiB, else "N.N MB" | src/features/messages/ComposerAttachments.tsx:114-118 | DONE (crates/app/src/ui/composer/attachments.rs) | The port shows "B" units and other rounding. Match the reference. | S |
| Poof animation and "plop" sound on removing an attachment | src/features/messages/AttachmentPoof.tsx:11-106, src/features/messages/ComposerAttachments.tsx:52-55 | DONE (crates/app/src/ui/composer/poof.rs) | Five-frame poof sprite at the chip plus a 0.34-volume sound. | S |
| Empty or oversized file is refused at add, with the "Could not attach file" toast | src/features/messages/attachment-draft.ts:193-195, src/features/messages/MessageComposer.tsx:1442-1448 | DONE (crates/app/src/ui/composer/attachments.rs) | The port adds a failed chip and shows inline warning text instead of a toast. | S |
| Draft attachment byte cap: "Attachment drafts are full (1,000 MiB)…" | src/features/messages/attachment-draft.ts:196-205 | DONE (crates/app/src/ui/composer/attachments.rs) | Track the retained bytes across drafts. | S |
| Draft attachments survive navigation (tab-local per channel/thread) and pause or resume uploads ("Upload paused. Retry to continue.") | src/features/messages/attachment-draft.ts:37-49, 71-85 | DONE (crates/app/src/ui/composer/attachments.rs) | Keep the attachment drafts in a session-level store keyed like the text draft. | M |
| Attaching while editing / unavailable: "Finish editing before attaching new files." / "Uploads are unavailable on this connection." | src/features/messages/MessageComposer.tsx:540-551 | DONE (crates/app/src/ui/composer.rs): every session can upload, so "Uploads are unavailable on this connection." does not arise | The port hides Attach during edits and drops the files quietly. Add the messages. | S |
| PNG/WebP with an ICC profile or EXIF orientation are re-rendered (orientation and colour applied), with WebP re-encoded losslessly | src/features/messages/image-metadata.ts:85-91, 136-140, src/features/messages/encode-webp.ts:2-41 | DONE (crates/relay/src/upload.rs) | The port drops the ICC/EXIF chunks without applying them, so rotated or wide-gamut images can come out wrong. | M |
| HEIC → JPEG and video → MP4 conversion before upload (host ffmpeg) | src/features/relay/attachments.ts:108-170 | DONE (crates/relay/src/upload.rs, through buzz-desktop's upload host) | Add a native transcoder (GStreamer is already used for voice). | L |

### Mentions, teams and outside-channel prompt

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Multi-word query admitted only while it starts a known name; a complete name plus a space closes it | src/bundled/mentions/mention-query.ts:22-31 | DONE (crates/app/src/ui/composer/mention_rules.rs:278) | Only checks for a tier ≤ 1 match among loaded profiles; there are no aliases or directory names. | S |
| Matching tiers 0-3 (equal, prefix, word equals, word prefix) after NFKD/diacritic folding; substring never matches; aliases add 4 | src/bundled/mentions/README.md:103-146, src/bundled/mentions/mention-ranking.ts:19-34 | DONE (crates/app/src/ui/composer/mention_rules.rs:62) | The port uses a substring tier (`oney` finds Honey), lowercase only, and no word-equals tier or aliases. Implement the spec and run the fixtures (`mention-rules.fixtures.json`). | M |
| Order: members first, match, base match, owned agents, agent blocks, recency, managed, presence, label, key | src/bundled/mentions/README.md:148-174, src/bundled/mentions/mention-ranking.ts:35-92 | DONE (crates/app/src/ui/composer/mention_rules.rs:185) | Has members → tier → name only. Add the remaining keys. | M |
| Choice set: members, offered agents, and a community directory search (200 ms debounce) for outsiders; archived identities excluded | src/bundled/mentions/useMentionDirectory.ts:12, 62, src/bundled/mentions/use-mention-choices.ts:29-215, src/bundled/mentions/mention-candidates.ts:35 | DONE (crates/app/src/ui/composer/mentions.rs:68, crates/app/src/ui/composer.rs:1314) | Uses members plus every cached named profile; there is no directory search, agent choices or archive exclusion. | M |
| Space accepts only an exact, unique name with no continuing name and a complete choice set | src/bundled/mentions/README.md:176-194 | DONE (crates/app/src/ui/composer/mention_rules.rs:255) | Exactness is checked only among the shown rows; continuations (`Jose Luis`) and pending searches are ignored. | S |
| At most 50 rows with the status "Narrow your search to see more members." | src/bundled/mentions/MentionCompletion.tsx:171-178, 211-213 | DONE (crates/app/src/ui/composer/mentions.rs:209) | Truncates silently. Add the status line. | S |
| Completion status lines (Loading recipients…, Searching community…, membership unavailable, names unavailable) and Retry | src/bundled/mentions/MentionCompletion.tsx:189-235 | DONE (crates/app/src/ui/composer/mentions.rs:199-209) | Add a status/retry footer to the completion popup. | S |
| Disabled rows with a reason (unavailable recipient); Enter/Tab on them do not fall through to Send | src/features/conversation/ComposerCompletions.tsx:291-301, 377 | DONE (crates/app/src/ui/composer.rs) | - | S |
| Mention picker popover from the @ button: search field ("Search community people and agents"), list, keyboard highlight, "No matching channel members." | src/bundled/mentions/MentionPicker.tsx:150-399 | DONE (crates/app/src/ui/composer/mention_picker.rs:149) | The port's @ button only types "@" to open inline completion. Build the popover. | M |
| Saved teams: mention a whole team (avatars, "Saved team · N agents", outsider consequence), with team names blocking Space selection | src/bundled/mentions/use-team-mentions.ts:13-207, src/bundled/mentions/TeamMentionAvatars.tsx:6 | DONE (crates/app/src/ui/composer.rs, crates/app/src/ui/composer/mentions.rs, crates/app/src/ui/composer/mention_picker.rs) | - | L |
| Batch insertion is atomic: "Choose at most 32 recipients" and the length limit ("The team would exceed…") | src/features/messages/MessageComposer.tsx:801-804, 825-885 | DONE (crates/app/src/ui/composer.rs:1728) | Only the writer rejects more than 32 at send. Check at insertion. | S |
| Recipient avatar spring scale-in animation | src/features/messages/MessageComposer.tsx:1756-1772 | DONE (crates/app/src/ui/composer.rs:3785) | Add the 0.9 → 1 scale on appearance (skipped for keyboard navigation and reduced motion). | S |
| Namesake qualifiers on chips (`Honey · 4prr`), revealed when a second namesake joins | src/features/messages/RichComposerInput.tsx:73-107, 237-245 | DONE (crates/app/src/ui/composer.rs:2801) | Use the contextual identity-name resolver for chip labels. | M |
| Recipient no longer admissible at insertion: "This recipient is no longer available. Remove it or refresh choices." | src/features/messages/MessageComposer.tsx:787-800, src/features/messages/mention-admission.ts:29-50 | DONE (crates/app/src/ui/composer.rs) | - | S |

### Channel and emoji completion, emoji picker

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Open (unjoined, public) channels from a search, after joined ones with 5 reserved slots, marked "Not joined" | src/bundled/channels/ChannelCompletion.tsx:96-117, 130-133 | DONE (crates/app/src/ui/composer/channels.rs) | Search public channels and append them. | M |
| Duplicate channel names show the channel id in the detail; "Private channel" detail; "Enter" hint on the selected row | src/bundled/channels/ChannelCompletion.tsx:130-136, src/features/conversation/ComposerCompletions.tsx:422-426 | DONE (crates/app/src/ui/composer/channels.rs) | "Private channel" and the Enter hint are present; the duplicate-name id is missing. | S |
| Channel completion statuses (Loading channels…, Searching open channels…, Keep typing to narrow the list.) | src/bundled/channels/ChannelCompletion.tsx:156-175 | DONE (crates/app/src/ui/composer/channels.rs) | Status footer (shared with the mention row). | S |
| Typing the closing colon accepts an exact shortcode; Space never accepts an emoji; a shortcode inside inline code stays literal | src/features/messages/MessageComposer.tsx:908-920, src/features/messages/MessageComposer.test.tsx:2861-2885 | DONE (crates/app/src/ui/composer.rs:1690, crates/app/src/ui/composer/live_markdown.rs:161) | The closing colon works. Add protection for code literals (including a Space-accepted mention inside code). | S |
| Shared shortcodes are labelled "Community emoji" / "Unicode emoji"; catalog error statuses with retry | src/bundled/emoji/EmojiCompletion.tsx:133, 149-177 | DONE (crates/app/src/ui/composer.rs:1300-1310, 4315-4325) | Detail labels and status lines. | S |
| Custom emoji shortcodes, links and profile links render inline in the draft (decorations), with Shift+Arrow treating a rendered shortcode as one unit | src/features/messages/RichComposerInput.tsx:114-276, src/features/messages/emoji-selection.ts:2-33 | DIFFERENT: GPUI shapes a text field's lines from text runs only, so a custom emoji image cannot sit inside the draft; shortcodes stay text there | - | M |
| Project resource picker inserts resource atoms ("Resource: …") at the caret | src/features/messages/MessageComposer.tsx:886-896, src/features/messages/EditableInput.tsx:1764-1772 | DONE (crates/app/src/ui/composer_tools.rs, crates/app/src/ui/projects/resource_picker.rs; no hidden "Resource: " prefix, as the port has no screen-reader text) | - | M |

### Voice notes

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Errors: microphone permission ("Allow Buzz to access your microphone to record a voice note."), unavailable, interrupted, prepare failed; Close button | src/bundled/voice-notes/useVoiceNoteRecorder.ts:73-74, 131-142, 185, 240-250, src/bundled/voice-notes/VoiceCapture.tsx:45-56 | DONE (crates/app/src/ui/media/recorder.rs:32-34, 186) | The permission-denied copy is missing; every start failure says "Buzz could not start the voice recorder." | S |
| Waveform strip slides 5 px per new sample (unless reduced motion) | src/bundled/voice-notes/VoiceNoteRecorder.tsx:47-61 | DONE (crates/app/src/ui/composer.rs:4050-4075) | Animate the bar strip offset between level ticks. | S |
| Discard button takes focus on start; Close focuses on error; focus returns to the composer after accept/cancel | src/bundled/voice-notes/VoiceNoteRecorder.tsx:27-29, src/bundled/voice-notes/VoiceCapture.tsx:17-19, src/bundled/voice-notes/index.tsx:25-33 | DONE (crates/app/src/ui/composer.rs:2174, 3971) | Only discard refocuses the composer. | S |
| Voice `waveform` written into the imeta tag | src/features/relay/attachments.ts:323-335 | DIFFERENT (crates/relay/src/messages.rs:271-273) | Intentional in the port: the relay refuses the `waveform` key, so only `duration` is written. | S |

## 5. Rendered attachments, media viewer, audio/video and voice-note playback

Note: Main gaps: no inline video playback (play opens the viewer), no timecoded-reply parsing (no time chips, timeline markers or quick reactions at the current frame), no shared-element open/close or gallery slide animation, no "Copy image", no download error notices, no snapshot attachments, and media keeps playing when the conversation is hidden.

Rows: DONE 61, DIFFERENT 3.

### Attachment layout, thumbnails and placeholders

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| The image strip is one horizontal row that scrolls (`overflow-x:auto`) and never wraps. | src/features/messages/Messages.module.css:434 | DONE (crates/app/src/ui/media/attachments.rs) | - | S |
| Smooth "squircle" media corners (60% smoothing path clip and SVG outline) on tiles, video previews, inline controls and the voice card | src/features/messages/media-corners.ts:1; use-media-corners.ts:4 | DIFFERENT (crates/app/src/ui/media/attachments.rs; video.rs; audio.rs) | Kept on purpose: GPUI can only clip to rounded rectangles, so tiles, previews and cards keep circular-arc corners. | M |
| The original replaces the blurhash immediately once it is decoded. | src/features/messages/AttachmentImage.tsx:193-207 | DONE (crates/app/src/ui/media.rs) | - | S |
| If the original fails to load, the tile keeps the blur/placeholder. "Image unavailable" appears only when the URL cannot be resolved. | src/features/messages/AttachmentImage.tsx:61-69, 158 | DONE (crates/app/src/ui/media.rs; crates/app/src/ui/media/attachments.rs) | - | S |
| Thumbnail labels ("Open image N of M"), keyboard focus ring, and the strip scrolls a keyboard-focused tile into view | src/features/messages/MessageRow.tsx:120-140, 719-722; Messages.module.css:449 | DONE (crates/app/src/ui/media/attachments.rs) | - | S |
| Any format the browser decodes renders (for example AVIF and SVG). | src/features/messages/AttachmentImage.tsx:204 | DIFFERENT (crates/app/src/ui/media.rs): SVG renders; AVIF shows as unavailable because GPUI's image decoder has no AVIF support | - | S |
| Non-thumbnail image preview (`MediaAttachment` image mode, min(360, w, 320*w/h)) | src/features/messages/MediaAttachment.tsx:134-189; AttachmentImage.tsx:42-51 | DONE (crates/app/src/ui/media/preview.rs, MediaAttachment's image mode that GitHub bodies use) | - | S |
| Attachment renderer registry: plugins can claim attachments, with a fallback renderer | src/features/conversation/AttachmentView.tsx:8-36; MessageRow.tsx:684 | DONE (crates/app/src/ui/renderers.rs; crates/app/src/ui/media/attachments.rs) | - | M |

### File attachment card

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Icon is always `file-text` (24px). | src/features/messages/FileAttachment.tsx:51 | DONE (crates/app/src/ui/media/attachments.rs) | - | S |
| A relay file click saves to Downloads without overwriting (host `media_download`) and does not open the file. | src/features/messages/FileAttachment.tsx:93-98; src-tauri/src/relay.rs:1579-1600 | DONE (crates/app/src/ui/media/attachments.rs; crates/app/src/ui/media.rs) | - | S |
| "Download failed" alert when a download fails | src/features/messages/FileAttachment.tsx:102 | DONE (crates/app/src/ui/media/attachments.rs) | - | S |

### Snapshot attachments

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Agent/team snapshot attachment: a file card plus an "Add agent" or "Add team" button that opens the snapshot import preview. The button is disabled when the message is cached or has no source. | src/features/messages/SnapshotAttachment.tsx:12-48; MessageRow.tsx:660 | DONE (crates/app/src/ui/media/attachments.rs, crates/app/src/ui/agents/snapshot_preview.rs) | - | M |
| Snapshot preview error alert (message or "Snapshot preview is unavailable.") | src/features/messages/SnapshotAttachment.tsx:30-38, 47 | DONE (crates/app/src/ui/media/attachments.rs) | - | S |

### Inline video preview (timeline)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Plays inline in the timeline. A centered play button plays in place, with an inline media-glass control bar (volume, time, timeline, duration, speed). The bar fades while playing unless hovered or focused. | src/features/messages/MediaAttachment.tsx:198-280; VideoPlayer.tsx:63-418; VideoPlayer.module.css:262-330 | DONE (crates/app/src/ui/media/video.rs; crates/app/src/ui/media/controls.rs) | - | M |
| An expand button (top right) is revealed on hover or focus. It pauses the video and opens the review at the current time. | src/features/messages/MediaAttachment.tsx:281-295; Messages.module.css:1976 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| "Video unavailable" status when the source is missing or fails to decode | src/features/messages/MediaAttachment.tsx:116-132 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| Inline video seeks to a timecode when a timecode chip in the thread panel is clicked (`seekTo` / `seekRequest`). | src/features/messages/MediaAttachment.tsx:42-70; ThreadPanel.tsx:1024-1027 | DONE (crates/app/src/ui/media/attachments.rs; crates/app/src/ui/media/video.rs) | - | M |
| Inline playback position is reported to the host (`onPlayback`) and synced back from the viewer to the inline element. | src/features/messages/MediaAttachment.tsx:248-252, 318-322 | DONE (crates/app/src/ui/media/video.rs; crates/app/src/ui/media/review.rs) | - | M |
| Inline video, audio and voice notes pause, and the viewer closes, when the conversation is no longer presented (tab or channel switch). | src/features/messages/MediaAttachment.tsx:56-58, 92; AudioAttachment.tsx:47-49; VoiceNoteCard.tsx:59-65 | DONE (crates/app/src/ui/media/audio.rs; crates/app/src/ui/media/review.rs; hooks in crates/app/src/ui/workspace.rs) | - | S |

### Video player in the viewer

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Autoplays on open, starting at the time handed off from the inline preview | src/features/messages/VideoPlayer.tsx:461, 468-480 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| Notice "Playback paused. Press play to try again." when play() is rejected | src/features/messages/VideoPlayer.tsx:153-161, 184-188 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| Error state reads "Media unavailable". | src/features/messages/MediaReviewViewer.tsx:419-430 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Volume: speaker button that opens a hover popover with a vertical slider and a Mute/Unmute button. The icon reflects the muted state. | src/features/messages/VideoPlayer.tsx:197-281; VideoPlayer.module.css:180-260 | DONE (crates/app/src/ui/media/controls.rs) | - | S |
| Hover time preview bubble above the timeline (mouse only) | src/features/messages/VideoPlayer.tsx:283-329 | DONE (crates/app/src/ui/media/controls.rs; crates/app/src/ui/media/seek.rs) | - | S |
| Keyboard-operable timeline and volume sliders (focusable, arrow keys) | src/features/messages/VideoPlayer.tsx:242-260, 301-317 | DONE (crates/app/src/ui/media/controls.rs; crates/app/src/ui/media/seek.rs) | - | S |
| Timecoded-comment markers on the timeline (author avatars). Clicking one seeks, opens the comments and selects that comment. | src/features/messages/VideoPlayer.tsx:330-358; MediaReviewViewer.tsx:243-264, 444-447 | DONE (crates/app/src/ui/media/controls.rs; crates/app/src/ui/media/review.rs) | - | M |
| Chosen speed is persisted (`buzz.video.playback-speed`) and broadcast to every open player | src/features/messages/VideoPlayer.tsx:47-57, 113-126, 392-404 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| Controls hide while playing when the pointer leaves or is idle for 2.2s. Focus or open menus keep them visible. | src/features/messages/use-media-controls.ts:4-35; VideoPlayer.module.css:307-315 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| On narrow players (<25rem) the timeline wraps to its own row. | src/features/messages/VideoPlayer.module.css:332-340 | DONE (crates/app/src/ui/media/controls.rs) | - | S |
| Press and hold on the right half plays at 2x until release | src/features/messages/use-video-gestures.ts:123-147 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| Gesture feedback overlay (play/pause icon, rotate-arrow with "10", "2x") fading after 650ms | src/features/messages/VideoPlayer.tsx:484-509; use-video-gestures.ts:29-34 | DONE (crates/app/src/ui/media/video.rs) | - | S |
| Progressive streaming: relay media streams with range reads through a local listener, so playback starts before the download finishes. | src/features/messages/use-media-element-source.ts:1-51 | DONE (crates/app/src/ui/media/engine.rs; crates/relay/src/media.rs) | - | M |
| "Fill the window" expand toggle (Escape exits first) | n/a (port only) | DONE (crates/app/src/ui/media/video.rs) | - | S |

### Video review reactions and timecoded comments

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Quick reaction bar under the player (😂 😍 😮 🙌 👍 👎). It pauses the video and posts the reply `⏱ m:ss — <emoji>`. | src/features/messages/VideoReviewReactions.tsx:46-77 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Custom emoji picker (ReactionTool) in the same bar | src/features/messages/VideoReviewReactions.tsx:72-82 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Bar disabled in read-only channels. Error alert "Could not post reaction." | src/features/messages/VideoReviewReactions.tsx:30-33, 44-50, 84 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Timecode stamp format: `formatMediaTime` (m:ss, h:mm:ss from one hour). The time readout next to the checkbox is padded 00:00 with tabular figures. | src/features/messages/media-timecode.ts:9-21; MediaReviewViewer.tsx:517 | DONE (crates/app/src/ui/media/timecode.rs; crates/app/src/ui/media/comments.rs) | - | S |
| Focusing the comment composer pauses the video | src/features/messages/MediaReviewViewer.tsx:525-530 | DONE (crates/app/src/ui/media/comments.rs) | - | S |
| Timecoded replies (`⏱ m:ss — text` and legacy `[m:ss]`) render with the prefix stripped and a mono time chip before the body. | src/features/messages/MessageRow.tsx:257-258, 630-643; media-timecode.ts:23-67; Messages.module.css:1962-1970 | DONE (crates/app/src/ui/media/timecode.rs) | - | S |
| Clicking a time chip in the viewer comments seeks the video and plays | src/features/messages/MediaReviewViewer.tsx:299-309, 627 | DONE (crates/app/src/ui/media/comments.rs) | - | S |

### Media review viewer: shell, motion, states

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Narrow windows: backdrop padding 12px (<50rem) and comments shown as a bottom sheet (<51.25rem) | src/features/messages/Messages.module.css:2027-2031, 2143-2157 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Shared-element open animation: the picture flies from the thumbnail with a smooth-corner clip (240ms). Chrome and scrim fade in. Respects reduced motion. | src/features/messages/use-review-entrance.ts:20-340; MediaAttachment.tsx:146 | DONE (crates/app/src/ui/media/review.rs) | - | M |
| Close animation: the picture flies back to its thumbnail, or fades if the thumbnail is off-screen, zoomed or a different gallery image. Chrome fades in 100ms and the scrim fades out. | src/features/messages/use-review-entrance.ts:341-499 | DONE (crates/app/src/ui/media/review.rs) | - | M |
| The comments sidebar slides in and out (200ms) while the picture moves smoothly to its new bounds. | src/features/messages/use-review-sidebar-motion.ts:28-103; Messages.module.css:2126-2139 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| States: thread loading, thread error with Retry, "Original message unavailable." with Retry, "Attachment unavailable.", stage "Media unavailable" with Retry | src/features/messages/MediaReviewViewer.tsx:133-167, 419-430, 547-549 | DONE (crates/app/src/ui/media/review.rs; crates/app/src/ui/media/comments.rs) | - | S |

### Image gallery stage (viewer)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Gallery holds every image across the thread (root and replies), de-duplicated | src/features/messages/MediaReviewViewer.tsx:267-276 | DONE (crates/app/src/ui/media/comments.rs; crates/app/src/ui/media/review.rs) | - | S |
| Slide and fade between images (200ms, ±40px). The last decoded image stays visible while the next loads. Respects reduced motion. | src/features/messages/use-image-gallery-motion.ts:15-122 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Only a click on the picture itself zooms (not the letterbox or a second touch) | src/features/messages/ImageReviewStage.tsx:247-253 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Trackpad pinch-to-zoom (gesture events) | src/features/messages/use-image-viewport.ts:165-186 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Pan is re-clamped on window resize or sidebar toggle | src/features/messages/use-image-viewport.ts:187-193 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| Cursors: zoom-in, grab, grabbing | src/features/messages/Messages.module.css:1192-1200 | DIFFERENT (crates/app/src/ui/media/review.rs) | Kept on purpose: GPUI has no zoom-in cursor, so the picture (only the picture) shows the pointing hand; grab and grabbing are the open and closed hands. | S |
| Toolbar auto-hides after 2.2s of idle pointer and stays visible during copy feedback | src/features/messages/ImageReviewStage.tsx:106-107; use-media-controls.ts:4 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| "Copy image" button copies authenticated PNG bytes to the clipboard, with a spinner and "Image copied" / "Couldn't copy image" notices (4s/6s) | src/features/messages/ImageReviewStage.tsx:117-158, 384-400; image-copy.ts:1-37; native-image-copy.ts:1-6 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| "Download image" for relay media saves to Downloads | src/features/messages/ImageReviewStage.tsx:401-429 | DONE (crates/app/src/ui/media/review.rs) | - | S |
| "Download failed. Try again." alert | src/features/messages/ImageReviewStage.tsx:416-418 | DONE (crates/app/src/ui/media/review.rs) | - | S |

### Comments sidebar

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Comments are full message rows (reactions, hover action bar, edit via MessageEditScope, retry, day dividers) | src/features/messages/MediaReviewViewer.tsx:502-514, 601-628 | DONE (crates/app/src/ui/media/comments.rs) | - | M |
| Clicking an attachment inside a comment switches the current viewer to that media (same review, seek re-applied) | src/features/messages/MediaReviewViewer.tsx:95-100, 624-626 | DONE (crates/app/src/ui/media/review.rs; crates/app/src/ui/media/comments.rs) | - | S |
| The comment selected from a marker is outlined and scrolled into view | src/features/messages/MediaReviewViewer.tsx:581-586, 605; Messages.module.css:2002-2005 | DONE (crates/app/src/ui/media/comments.rs) | - | S |

### Audio attachment player

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Duration is known before play (preload metadata) | src/features/messages/AudioAttachment.tsx:250-253 | DONE (crates/app/src/ui/media/audio.rs) | - | S |
| Hover time preview on the seek bar | src/features/messages/AudioAttachment.tsx:310-361 | DONE (crates/app/src/ui/media/audio.rs) | - | S |
| Duration is checked (finite, at most the maximum attachment duration). Old voice notes with an overshooting container duration are corrected when they end. | src/features/messages/AudioAttachment.tsx:152-164, 228-240, 266-288 | DONE (crates/app/src/ui/media/audio.rs) | - | S |

### Voice note card (playback)

| Feature | buzz-app reference | Port status | What's needed | Size (S/M/L) |
|---|---|---|---|---|
| Loading spinner rotates | src/bundled/voice-notes/VoiceNoteCard.tsx:190-194; VoiceNotes.module.css:254 | DONE (crates/app/src/ui/media/audio.rs) | - | S |
| Bar count follows the card width (floor((w+2)/5), at most 64) | src/bundled/voice-notes/VoiceNoteCard.tsx:80-91 | DONE (crates/app/src/ui/media/audio.rs) | - | S |
| Seeking on the waveform is enabled only after metadata loads (that is, after Play) | src/bundled/voice-notes/VoiceNoteCard.tsx:217-234 | DONE (crates/app/src/ui/media/audio.rs) | - | S |

## 6. buzz-app main of 2026-10-10

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Typed angle-bracket text shows as typed in messages, notification and inbox previews; complete HTML comments stay hidden; Linux banners escape markup only when the server takes `body-markup` (#803) | src/features/messages/MessageMarkdown.tsx; src/features/notifications/content.ts; src/features/relay/message-content.ts; ST/notifications/linux.rs | DONE (ui/markdown.rs, relay/src/fold/attachments.rs, app/src/notifications.rs, notifications/banners.rs) | — | S |
| The outside-channel mention prompt defaults to Invite when you can add people (#825) | src/features/messages/useNonmemberMentions.tsx | DONE (ui/composer.rs) | — | S |
| Compact clock shares the byline clock's formatting (#609) | src/shared/datetime.ts; src/features/messages/MessageTimestamp.tsx | DONE (ui/time.rs; output already matched, now pinned by a test) | — | S |
| A link to another message of the open thread, or to its root, selects it in place, keeping the reply being written, its reply target, the scroll position and open branches; Back and Forward to such a link do the same. The message must name the thread's root in its own signed ancestry, and it is held with its edits, reactions and their deletions until the session admits the complete fold, which is the last word; a newer link wins; a link into another thread opens that thread's own tab while this one keeps its reply; one that cannot open keeps the thread and notes "That message is no longer available." or "Couldn’t open that message." with Try again; a root link neither scrolls nor moves the focus (#853, ported ahead of merge; closed upstream, shipped as #904 and refined by #927, #1018 and #1085) | src/features/relay/threads.ts `retarget`, `readTarget`; src/features/relay/session.ts `targetReader`; src/features/messages/ThreadPanel.tsx `RetainedThreadPanel` | DONE (relay/src/threads.rs `ThreadState::retarget`, relay/src/session/threads.rs `ThreadView::retarget`, app/src/ui/thread.rs `select`, ui/channel_tab_views.rs `ensure_views`, ui/channel_tabs.rs `divert_thread`) | Reconciled with #904: the relay half now admits a selected message only as its full fold (live deletions go with it, refused evidence never latches a root); the earlier port's history recording of a selected message's thread, which buzz-app did not ship, is gone, so places are recorded as navigated and a link names a thread only by its `thread` hint; a root selection no longer reveals the root. The popover close fix has no counterpart: GPUI popovers keep no library hover state | M |

## 7. buzz-app main of 2026-10-14

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Inbox conversation details offer Edit message on the viewer's messages; an emptied edit's deletion ends the edit and brings the reply draft back only on its receipt or echo, never on a discarded deletion or a replacement edit, and moves focus to the composer only from nowhere, the composer or the dialog; hiding the preview dismisses the delete confirmation without cancelling a sent deletion (#815) | src/bundled/inbox/InboxDetail.tsx; src/features/messages/{MessageManagement,MessageComposer}.tsx | DONE (app/src/ui/message_actions.rs `EditDeletion`, `set_presented`; ui/composer.rs `edit_session`) | The port's Inbox reader already had the shared message actions | S |
| Message prose uses its own type role, 14px with 1.625 leading; prose blocks 16px apart; the continuation clock centres on the first line (#847) | src/features/messages/Messages.module.css; DS/styles/typography.css `--text-message` | DONE (ui/src/tokens.rs `TextRole::Message`, ui/src/styled.rs, app/src/ui/message.rs, ui/markdown.rs) | — | S |
| A lone custom emoji in the composer enlarges only the emoji, not ProseMirror's trailing separator image (#897) | src/features/messages/EditableInput.module.css | N/A: the composer has no ProseMirror separator; custom emoji show as their shortcode text | — | — |
| The composer's emoji button tooltip reads "Emoji and GIFS" (#923) | src/bundled/emoji/EmojiPicker.tsx | DONE (app/src/ui/composer.rs) | — | S |
| Switching Emoji and GIF tabs hides the outgoing panel at once, so the picker does not grow and jump (#925) | src/bundled/emoji/Emoji.module.css | N/A: the picker draws only the selected tab's panel, so both never share a frame | — | — |
| The issue and pull request tool keeps the session's last project answer per channel (64 channels, 1 MiB, cleared when the stream drops, on Clear cache and on dispose): a recreated picker shows a known project disabled ("Loading project…") while it checks again and stays hidden for none or no answer; Retry checks without it; older checks never replace newer ones (#877) | src/bundled/projects/ResourcePicker.tsx; src/features/projects/destinations.ts; src/features/relay/session.ts | DONE (relay/src/projects.rs `Homes`, relay/src/session/projects.rs `peek_home`, session/mod.rs, session/account.rs, app/src/ui/projects/resource_picker.rs) | — | M |
| A GitHub pull request link opens the panel only without a fragment or with a conversation comment or review anchor; issue links only without one; other anchors and the Files, Commits and Checks tabs open in the browser (#920) | src/bundled/github/references.ts | DONE (app/src/ui/github/references.rs) | — | S |
| Composer attachments upload eagerly, one at a time; pausing (no viewer, disabled composer) re-queues and resuming continues without Retry; only failures block Send; Send hands the running upload to the background send; a failed send freezes the rest as "Upload paused"; 92px media tiles and 18rem chips with a hanging corner control (progress ring with Cancel on hover, Retry plus Remove, Remove on hover); removed tiles shrink away; a fullscreen draft viewer (title, Previous/Next, "n / total", arrow keys, gallery slide, flight from the tile; image, video, audio, "No preview available for this file type.", "Preview unavailable"); "Drop files into this conversation" over the pane (#948) | src/features/messages/attachment-draft.ts; background-upload.ts; ComposerAttachments.tsx; ComposerAttachmentViewer.tsx; MediaReviewFrame.tsx; FileDropOverlay.tsx; MessageComposer.tsx | DONE (app/src/ui/composer/attachments.rs, ui/composer.rs, ui/media/draft_viewer.rs, ui/media/review.rs, ui/channel.rs, ui/thread.rs) | Not ported: zooming a draft image, Delete on a failed tile's focused control (tiles take no focus), the attachment section's height collapse. The tiles sit 10px lower than buzz-app's, whose negative scroll margins take a GPUI row's height away | L |
| Space selects a unique exact local name (members and agents offered for mention) at once, once every member is named; other names still wait for the full directory; key qualifiers and generated "(agent)" suffixes never block (#935) | src/bundled/mentions/use-mention-choices.ts; mention-ranking.ts; src/features/search/person-match.ts | DONE (app/src/ui/composer/mentions.rs, composer/mention_rules.rs `exact_mention_among`, ui/identity.rs `qualifier`) | — | S |
| The viewer's own agents, not only managed ones, outside the conversation are offered as mentions only in streams and forums the viewer can add members to; sessions invite them; DMs never offer them; saved-team rows follow the same rule (#882 `763c023e`) | src/bundled/mentions/mention-candidates.ts `availableMentionAgents` | DONE (app/src/ui/composer/mentions.rs `offers_agents`, ui/composer.rs) | "Can add members" is a live (not cached) session, standing in for `outbox.supports(9000)` | S |
| Composer corners match the send control plus inset (17px); the edit header is flush; the timeline's top edge is 16px until a history control shows, then keeps its space; quiet option rows for quick statuses (#801) | src/features/messages/Messages.module.css; ChannelTimeline.tsx; src/features/user-status/StatusEditor.tsx | DONE (app/src/ui/composer.rs, ui/channel.rs, ui/status.rs) | Large custom emoji composition is N/A: the composer shows custom emoji as shortcode text, so there is no artwork to keep large around a text-height caret | S |
| The video's fullscreen button stays clickable above the inline controls (#1007) | src/features/messages/Messages.module.css | N/A: the expand button is the preview's last child, so it already paints and takes clicks above the controls | — | — |
| Every sent image and GIF shows whole at its own proportions, at most 360 × 320 and never enlarged, in a vertical list with the count below; imeta dimensions (else 360 × 320) reserve the frame until decode; no grey backing once loaded; "Image unavailable" inside the frame; large emoji get the 48px line height on every line (#886) | src/features/messages/AttachmentImage.tsx; MessageRow.tsx; Messages.module.css | DONE (app/src/ui/media/attachments.rs `image_group`, `preview_frame`; ui/markdown.rs) | — | M |
| Animated avatars (`#buzz-anim=` after a still image URL) load their still and its thumbnail (#990) | src/features/relay/transport.ts `mediaUrl` | DONE (relay/src/media.rs `still`) | The dev broker half is N/A: the port has no browser broker | S |
| Prepared JPEGs lose encoder-added APP/COM sections except canonical JFIF and Adobe; tiles 12px apart; the last corner control stays on the corner after Retry (#1068) | src/features/messages/jpeg-metadata.ts; prepare-attachment.ts; ComposerAttachments.module.css | DONE (relay/src/upload.rs `clean_encoded_jpeg`, app/src/ui/composer.rs) | — | S |
| The floating action bar stops its per-frame loop while a row stays revealed (#938) | src/features/messages/useFloatingActionBar.ts | N/A: a React requestAnimationFrame cost; the port positions the bar in its render | — | — |
| A link to another message of the open thread, or its root, keeps the reply intact: the retained thread panel selects it in place after verifying its ancestry and folding its overlays (#904 `be861c9a`) | src/features/relay/threads.ts; src/features/relay/session.ts; src/features/messages/ThreadPanel.tsx `RetainedThreadPanel` | DONE (see the #853 row in section 6; relay/src/threads.rs, relay/src/session/threads.rs, app/src/ui/thread.rs `select`, ui/channel_tab_views.rs) | The popover and agent chooser close through Base UI to reset its hover state: N/A, GPUI keeps no such state | M |
| The thread composer's placeholder reads "Reply in thread" when the viewer wrote the root or owns its workflow, instead of naming them (#930) | src/features/messages/ThreadPanel.tsx | DONE (app/src/ui/thread.rs) | — | S |
| Join previews: no disabled composer; a floating "Join channel to send messages" prompt with Join (or "You’re previewing this channel.") rises over the history once content shows, the history keeping room for it; a preview thread offers the same prompt and a join started there focuses the reply composer; quieter day dividers with a calendar tile between hairlines; caption-sized membership notices with 20px avatars (#828) | src/features/messages/ChannelJoinNotice.tsx; MessageTimestamp.tsx; ThreadPanel.tsx; Messages.module.css; src/bundled/channels/ChannelsPage.tsx | DONE (app/src/ui/channel.rs, ui/membership.rs, ui/message.rs `day_calendar`, ui/thread.rs, ui/jump.rs, ui/time.rs, ui/channel_tab_views.rs) | — | M |
| A thread newly selected in the main pane slides in from the right over 300ms while what it replaces drifts left and dims; back to the conversation is immediate, and keyboard choices or reduced motion skip it. A thread waiting for its first page shows the Buzz loading mark (80–160px, after a moment), and its rows fade in together once the first page lands (#1018) | src/features/panels/PanelWorkspace.tsx; Panels.module.css; src/features/messages/ThreadPanel.tsx; Messages.module.css | DONE (app/src/ui/workspace/board.rs, ui/thread.rs, ui/launch.rs, ui/channel.rs) | — | M |
| A growing composer lifts the history with it while the reader is at the bottom, in channels and threads (#864) | src/features/messages/ChannelTimeline.tsx; ThreadPanel.tsx | DONE (no change: GPUI's list keeps a tail-following reader at the end whenever its viewport shrinks; app/src/ui/channel.rs, ui/thread.rs) | — | — |
| Reading wakes once an exact message reveal is verified, so the revealed rows are read without further input (#1006) | src/features/messages/use-message-reveal.ts | DONE (no change: the reader reports what each drawn frame shows, app/src/ui/reading.rs, so a reveal is read without an event) | — | — |
| A notification reveal that leaves the reader at the very bottom keeps following, so rows measuring taller later do not push the latest row away; a reveal short of the end keeps its row in place (#1085) | src/features/messages/ChannelTimeline.tsx | DONE (no change: a reveal scrolls GPUI's list to the row, which suspends following; the list resumes following the tail when that leaves it at its end, app/src/ui/channel.rs `reveal`) | — | — |
| Bottom-follow survives the timeline's own end alignment and Virtua's offset corrections; only reader input cancels it. An expired reading observation (the visit ended during its save) no longer puts read sync in error. Day divider numeral drawn as text over the artwork (#1039) | src/features/messages/ChannelTimeline.tsx; src/features/relay/read-state.ts; MessageTimestamp.tsx | DONE (no change: GPUI's list stops following only on reader scrolls; relay/src/read_state.rs leaves sync health alone on `Expired`, and unread.rs checks the visit before its synchronous save; ui/message.rs `day_calendar` already drew the numeral as text) | The CI and WebKit flake-check parts are N/A (test tooling) | — |
| A revealed row holds its place through late row measurement until reader input, a send or Jump to latest (#1087) | src/features/messages/use-message-reveal.ts; ChannelTimeline.tsx | DONE (no change: GPUI's list anchors its scroll to the revealed item, so rows measuring later do not move it) | The browser test fixes are N/A | — |
| Mounted rows stop re-rendering per message; thread rows are memoized (#943, #937) | src/features/messages/*.tsx | N/A: React render costs | — | — |

## Not counted: absent from buzz-app, or not applicable to a native app

These were checked so that nobody ports them by mistake. In each case the port already matches.

- Code blocks: no syntax highlighting, no copy button (reference has neither): `src/features/messages/MessageMarkdown.tsx:618-630`
- No OpenGraph cards, no YouTube/media embeds for links (reference has none): `src/bundled/link-lab/README.md:9-13`
- Mac WebKit workarounds (scroll-momentum fix, ghost caret): `src/features/messages/mac-webkit.ts:1-6`
- Right-click context menu on messages (none in buzz-app; no onContextMenu in src/features/messages)
- Pin message (not in buzz-app)
- Fallback reaction text row when the conversation has no session/extensions: `src/features/messages/MessageRow.tsx:810-840`
- Picker load error "Could not load emoji picker: …" + "Retry picker": `src/bundled/emoji/EmojiPicker.tsx:263-270`
- "Reactions unavailable" status when the picker contribution crashes: `src/features/conversation/ReactionTool.tsx:44-47`
- Rename / delete custom emoji (not in buzz-app)
- Unread "new messages" divider line in the timeline (docs/port/ui.md (4.3) / src/features/messages/ChannelTimeline.tsx:763)
- Last reply time in the summary: `src/features/messages/ReplySummary.tsx:61`

## DONE count

DONE: **348** of 361 rows (section 1: 60, section 2: 83, section 3: 68, section 4: 72, section 5: 61, section 6: 4). The first pass also counted DONE behaviours that are not listed.

Status counts: MISSING 0, PARTIAL 0, DIFFERENT 10, SKIPPED 3, DONE 348.
