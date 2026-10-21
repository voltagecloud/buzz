# Parity gap audit: channels, communities, DMs and the relay data layer

- Reference: buzz-app at `/home/ben/projects/buzz-app` (React/Tauri).
- Port: buzz-gpui at `/home/ben/projects/buzz-gpui`, main checkout only. `.claude/worktrees/` was not audited.
- Slice: `src/features/{channels,channel-members,channel-navigation,channel-templates,communities,direct-messages,relay}`; `src/bundled/{channels,channel-templates,channel-usage,hosted-communities,moderation,inbox,profiles}`; `src-tauri/src/relay*` and `src-tauri/src/enterprise_*`.
- Citations: buzz-app paths are relative to the buzz-app root. Port paths are relative to the buzz-gpui root (`crates/...`), unless a section says otherwise. Line numbers were read on 2026-10-09.
- Status: MISSING, PARTIAL (the gap is described), DIFFERENT (the difference is described), DONE. DONE rows have "-" in "What's needed".
- Duplicates: when a behaviour belongs to more than one area, it is listed once and the other areas point to it.

## Summary

| Area | DONE | PARTIAL | DIFFERENT | MISSING | SKIPPED | Rows |
|---|---|---|---|---|---|---|
| Connection, transport, native host, enterprise | 61 | 0 | 2 | 0 | 3 | 66 |
| Session, caching, windows, folding | 68 | 0 | 2 | 0 | 0 | 70 |
| Outbox, publishing, attachments, media | 59 | 0 | 5 | 0 | 0 | 64 |
| Read state and unread | 62 | 0 | 1 | 0 | 0 | 63 |
| Sidebar, discovery, navigation, DMs; members, moderation, profiles; communities, hosted, inbox | 134 | 0 | 0 | 0 | 1 | 135 |
| Other relay-layer features | 90 | 0 | 1 | 0 | 0 | 91 |
| Channel page, settings, lifecycle, create, canvas, templates, usage | 73 | 0 | 0 | 0 | 0 | 73 |
| **Total** | **547** | **0** | **11** | **0** | **4** | **562** |

Statuses were reconciled with the code on 2026-10-10. The findings below are from the original audit and most are closed.

### Highest-impact findings

1. **The sidebar preferences layer is not wired up.** `crates/relay/src/sidebar.rs` (3.7k lines) ports registers, edits, decoding, sections, local hidden DMs, activity and personal groups. Only its own tests call it. The UI sidebar is a fixed Channels / Forums / Direct messages list: no Starred, groups, sort, mute, collapse, drag, row context menu or resize.
2. **Nothing is persisted to disk.** No disk heads, startup roster cache or offline resume. `Store::save_head`/`load_head` and the `startup_cache` table have no callers. A community shows Connecting with an empty sidebar until NIP-11 succeeds.
3. **AUTH and reconnect behaviour differs.** After 1.5 s with no AUTH challenge, the port treats the relay as open and sends REQs (`crates/relay/src/socket.rs:348-351`). AUTH refusals and signing failures are not terminal. Reconnect retries forever with jitter instead of 5 attempts and then a manual retry. `retry()` clears the relay's quota cooldown.
4. **Connection and live status are invisible.** The snapshot exposes only `live: bool`. `Session::retry` has no caller in `crates/app`. There is no "Live updates need attention" toast, no failed-connection banner, and no sidebar connecting or error states.
5. **Some sends can be invisible.** Only signed outbox ops reach timelines (`crates/relay/src/session/mod.rs:1462-1468`). The outbox exists only after connect. (The outbox panel and failed edit, delete and reaction states are now done.)
6. **DMs are counted as mentions.** DM entries count as mentions (`crates/relay/src/unread.rs:1536-1538`). DM rows get a numeric pill, and the tray counts DMs twice. The sidebar also shows a numeric pill where buzz-app shows a dot.
7. **Whole features are absent:** community moderation (Membership card, invites), hosted communities, the inbox page, the profile panel, Canvas, channel templates, channel usage, reminders, identity archives, and the tab pane and panels.
8. **Some writes can be resent.** An unconfirmed lifecycle action can be sent again, and an uncertain channel-details save has no retained state. Renaming a session channel fails because the port always sends `about`. There is no temporary-channel TTL.

## Connection, transport and native relay host

Biggest gaps: no connection or live-status UI (session exposes only `live: bool`; nothing in `crates/app` calls `Session::retry`); NIP-42 AUTH failures are not terminal (a refused or failed AUTH loops forever, or after 1.5 s REQs go out unauthenticated); reconnect retries forever with jitter instead of 5 attempts then manual retry; several quota branches differ; offline cached startup is missing; several host features (observer route, workflow runs, git auth, channel kit, catalog guard, enterprise login gate) are absent. Paths: buzz-app `src/features/relay/` or `src-tauri/src/`; port `crates/relay/src/`.

### Session connection lifecycle

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Connect reads NIP-11: `self` must be 64-hex ("Relay did not advertise its identity"); NIP-29 enables channel creation; read-state snapshot descriptor | native.ts:315-342; src-tauri/src/relay.rs:610-638 | DONE (session/mod.rs:831-844, 1580-1605) | - | - |
| 8 s connect deadline "Relay connection timed out. Try again." | service.ts:187-197 | DONE (session/mod.rs:836-840) | - | - |
| A failed connect waits for user retry (or online/visible while cached) | service.ts:165-302 | DONE (session/mod.rs; app network.rs, main_window.rs) | - | - |
| OS `online` retries a cached session; becoming visible runs `refreshList()` | service.ts:341-363 | DONE (session/mod.rs `online`/`visible`; app network.rs, main_window.rs) | - | - |
| Native origin must be HTTPS without credentials/path/query/fragment | src-tauri/src/relay.rs:58-72 | DONE (origin.rs: secure origins only; plain ws/http is kept for loopback development relays) | - | - |

### Socket open and NIP-42 AUTH

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| One socket per (origin, viewer); URL `wss://host` | transport.ts:1172-1205; live.ts:204-214 | DONE (live.rs:157-174; origin.rs:293) | - | - |
| AUTH kind 22242 with exactly `relay` + `challenge`, challenge 1–4096 bytes | live.ts:720-747; src-tauri/src/relay.rs:260-285 | DONE (crates/identity/src/lib.rs:223-234; socket.rs:578-592) | - | - |
| Every REQ held until AUTH OK; never treated as open relay | live.ts:523, 748-760 | DONE (socket.rs) | - | - |
| 10 s auth deadline from socket creation ("Live authentication timed out") | live.ts:698-701 | DONE (socket.rs) | - | - |
| `OK false` for AUTH is terminal ("Relay rejected live authentication") | live.ts:748-752, 679-686 | DONE (socket.rs) | - | - |
| Signing failure / signer ≠ viewer is terminal | live.ts:736-745 | DONE (socket.rs) | - | - |

### Reconnect, liveness, recovery

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Backoff `500*2^n` ms, no jitter | live.ts:677 | DONE (socket.rs) | - | - |
| ≤5 auto-reconnects then "Live reconnect attempts exhausted; retry available"; counter resets only in `retry()` | live.ts:671-677, 1055-1058 | DONE (socket.rs; live.rs) | - | - |
| No client heartbeat | live.ts (absent); docs/port/relay.md:700-701 | DONE (socket.rs; kept and documented as a port addition) | - | - |
| On reconnect rebuild routes with `since=now-300`, `limit 500` | live.ts:653-660, 430 | DONE (live.rs:295-317, 408-421) | - | - |
| Live `retry()` keeps the server cooldown | live.ts:1055-1069, 24-59 | DONE (live.rs) | - | - |
| 60 s tick renews multi-channel batches with `limit:0` | live.ts:345-368 | DONE (live.rs:743-762) | - | - |
| Tick re-pends retained renewals and calls `recover()` (roster refresh + `unread.reconnect()`) | live.ts:353-367 | DONE (live.rs; session/mod.rs, session/unread.rs) | - | - |

### Subscription management

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Routes (profiles, membership, presence, singletons, batches of 10); capacity 1022; `limited` over capacity | live.ts:17-22, 369-454 | DONE (live.rs:40-54, 321-406) | - | - |
| REQ filters per route (channel kinds `#h`; profiles; membership incl. read-state and 30300; presence `limit 0`) | live.ts:200-203, 572-635 | DONE (live.rs:462-511) | - | - |
| Observer/telemetry route (24200/44200 `#p viewer`, archive capture, settings retry) | live.ts:302-320, 567-588, 852-866; native.ts:539-659 | DONE (crates/relay/src/session/observer.rs; crates/app/src/agent_activity/observer.rs decrypts with buzz-desktop; crates/app/src/ui/settings/archive.rs) | - | M |
| `prioritize` ≤64 ids; singletons; globals first; `SETUP_CONCURRENCY=4` | live.ts:22, 521-553, 1031-1035 | DONE (live.rs:214-226, 515-565) | - | - |
| Setup deadline 10 s ("Live subscription setup timed out; retry available"); keep previous wire and re-pend | live.ts:455-473, 558-565 | DONE (live.rs) | - | - |
| Replacement keeps previous wire until EOSE; previous-wire events filtered to narrowed scope | live.ts:323-344, 832-842 | DONE (live.rs) | - | - |
| Interest validation (≤1024 ids, id regex, presence authors 64-hex ≤256, throw not truncate) | live.ts:148-181 | DONE (live.rs) | - | - |
| Dropping a route fences its wire then sends CLOSE | live.ts:288-297 | DONE (socket.rs:167-171, 453-460; live.rs:436-441) | - | - |

### Frame handling

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Inbound frames over 1 MiB or non-string dropped | live.ts:707-710 | DONE (socket.rs) | - | - |
| Invalid live EVENT fails the route ("Relay supplied invalid live traffic") | live.ts:805-811 | DONE (socket.rs; live.rs) | - | - |
| Channel attribution incl. multi-`h` events delivered with `sourceChannels` | live.ts:812-879 | DONE (live.rs; session/timeline.rs) | - | - |
| Replay vs live phase tagging | live.ts:870-878 | DONE (live.rs:579-583) | - | - |
| EOSE: route live, previous wire closed, `established`; `replay:"limited"` when ≥500 | live.ts:880-892 | DONE (live.rs; `LiveSnapshot` routes) | - | - |
| CLOSED: reason ≤512 chars; "restricted: not a channel member" = denied | live.ts:504-518, 893-903 | DONE (live.rs:664-697) | - | - |
| CLOSED on previous wire closes only that wire | live.ts:797-802 | DONE (live.rs:613-622) | - | - |
| NOTICE logged; OK matched to AUTH, presence, publications | live.ts:718, 748-790 | DONE (socket.rs:547-597) | - | - |

### Live quota and rate limits

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Shared cooldown `pause(N)` = now+(N+1) s, gates REQ dispatch, survives socket replacement; ≤3 quiet retries for hint ≤60 s | live.ts:23, 26-59, 475-492 | DONE (live.rs:516, 675-683, 700-712) | - | - |
| Missing hint treated as 5 s and retried | live.ts:479 | DONE (live.rs) | - | - |
| Hint >60 s: all unsent pending routes error "Unsupported live cooldown; automatic setup stopped"; pause min(N, 86400) | live.ts:480-489 | DONE (live.rs) | - | - |
| Retries exhausted stops every unsent pending route | live.ts:492-501 | DONE (live.rs) | - | - |
| Transient CLOSED on renewal keeps wire and re-pends next tick | live.ts:455-472, 353-360, 898-902 | DONE (live.rs) | - | - |

### Socket publications and presence

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Socket publications (≤32 pending, ≤3 unacked, dup refused, 64 KiB frame, 10 s incl. pre-auth, receipt ≤16 KiB, `sent` kept) | socket-requests.ts:26-140 | DONE (socket.rs) | - | - |
| OK false proves non-delivery only for specific prefixes | socket-requests.ts:109-130 | DONE (socket.rs) | - | - |
| Publication `rate-limited` pauses the shared live cooldown | live.ts:778-790 | DONE (socket.rs) | - | - |
| Presence publish: 5 s gate, 10 s receipt timeout; no-hint quota pauses 86400 s | live.ts:922-985, 761-777 | DONE (socket.rs; presence.rs; session/presence.rs) | - | - |
| Presence snapshot over `/query` (relay-signed, one `p`, 10 s, 1 MiB, absent = offline) | transport.ts:310-353, 1131-1171 | DONE (session/presence.rs:115-147; presence.rs:137-180) | - | - |

### HTTP transport, NIP-98 and admission

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| No redirects, 10 s connect, 30 s total; NIP-11 GET with `Accept: application/nostr+json` | src-tauri/src/relay.rs:640-653, 708-769; native.ts:99-124 | DONE (http.rs:65-70, 164-180) | - | - |
| NIP-98 kind 27235 (`u`, `method`, `payload`, `nonce`) per request | src-tauri/src/relay.rs:717-756 | DONE (crates/identity/src/lib.rs:238-257; http.rs:121-131) | - | - |
| Size caps: 16 MiB response, 1 MiB request | src-tauri/src/relay.rs:40-41, 662-666 | DONE (http.rs) | - | - |
| One invalid event rejects the whole read | transport.ts:221-243 | DONE (reader.rs:538-549; `RelayHttp::query` drops silently, http.rs:91-99) | - | - |
| API lane: 6 concurrent per (origin, viewer) | http-admission.ts:148; host-admission.ts:6-38 | DONE (http.rs:49-61, 111-120) | - | - |
| Lane queue: foreground first, ≤128 queued ("Relay API queue capacity reached") | http-admission.ts:139-154, 173-182, 204 | DONE (http.rs) | - | - |
| 429 quota pauses lane N+1 s (no hint 61 s, max 86400), fails queued work "Relay requests paused; retry in Ns" | http-admission.ts:34-51, 188-196, 242-264 | DONE (http.rs:136-161, 229-237) | - | - |
| Presence lossy lease outside the 6 slots | http-admission.ts:157-169 | DONE (http.rs; session/presence.rs) | - | - |
| Error text (4 KiB body, known messages else "Relay request failed (N)", error kinds) | http-admission.ts:31-113; errors.ts:1-36 | DONE (reader.rs:60-125) | - | - |

### Write receipts

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|

### Error surfaces

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Live updates need attention" notice (inline above the Messages composer since #813) with reason and "Retry live updates"; quiet during quota recovery; diagnostics and "Last rejection" | src/bundled/channels/LiveStatus.tsx | DONE (session/mod.rs `LiveView`; app ui/live_status.rs; the diagnostics text is the channel settings Diagnostics row) | - | - |
| Relay connection error with Try again | service.ts:187-197, 290-295 | DONE (session/mod.rs; app ui/live_status.rs) | - | - |

### Native relay host

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Webview hardening (path allowlist, sign-kind allowlist, `/events` kind gates, shape checks) | src-tauri/src/relay.rs:74-94, 225-323, 654-706 | DIFFERENT | No untrusted webview; signing scoped by `Purpose` (crates/identity/src/lib.rs:396) | - |
| Workflow runs read `GET /workflows/{id}/runs?limit=20` | src-tauri/src/relay.rs:96-162; native.ts:401-426 | DONE (crates/relay/src/workflows/protocol.rs `runs_path`, crates/relay/src/workflows/capability.rs) | - | M |
| Git authorization (27235 token) and project git | src-tauri/src/relay.rs:164-223; native.ts:427-455, 702-710 | DONE (crates/identity/src/lib.rs `git_auth`; crates/relay/src/session/projects.rs) | - | M |
| Catalog sign guard (30175/30178, `env_vars` refused) | src-tauri/src/relay/catalog.rs:1-87 | DONE (crates/identity/src/lib.rs `Purpose::Catalog`, `check_catalog` with buzz-desktop `relay::events::validate_event`) | - | M |

### Enterprise

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Build-time enterprise relays / adapter URL validation and canonicalization (`wss://host/`, IDNA, HTTPS or loopback adapter) | src-tauri/build.rs:170-190; src-tauri/src/enterprise_auth_build.rs:10-42; enterprise_relay_url.rs:1-75; enterprise_adapter_url.rs:3-26 | SKIPPED (decided: enterprise login gate) | - | S |
| `enterprise_login_gate` (`GET /info` 128 KiB; Required only for federated client-attached offline-jwt; fails closed) | src-tauri/src/enterprise_login_gate.rs:35-121 | SKIPPED (decided: enterprise login gate) | - | M |

### Profiling, debug, budget

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Relay profiler (2048-sample ring, stages, Server-Timing) and client metrics | profiling.ts:1-94; transport.ts:247-283, 1361-1381 | SKIPPED (decided: Developer settings) | Every request is traced with timings and Server-Timing (http.rs `send`, target `buzz_relay::profile`). The 2048-sample ring and client metrics show only in buzz-app's Developer settings (`ClientMetricsPanel`), which this port does not have | S |
| Frame traffic log and `relayDebug` | live.ts:280-286, 689, 718; debug.ts:1-7 | DONE (socket.rs: `relay-ws` trace target for every frame) | - | - |
| `yieldToHost` batches of 12 | yield.ts:1-12; transport.ts:234-241 | DONE (reader.rs:544-547, `spawn_blocking`) | - | - |
| Budgets (32 KiB input, byteSize, ByteLru) | budget.ts:1-71 | DONE (outbox.rs:51, 749; unread.rs:319) | - | - |
| Dev browser broker transport and SSE live | transport.ts:356-1085; broker-live.ts:1-558 | DIFFERENT | Deliberately not ported (no browser host) | - |

## Session, caching, timeline windows and folding

Port fold engine (rows, ordering, incremental projection, attachments) matches buzz-app. The biggest gaps are persistence and caching: none of the disk-backed startup behaviour is wired up (cached heads, startup roster cache, offline resume). Paths: buzz-app `src/features/relay/`, port `crates/relay/src/`, unless prefixed.

### Session lifecycle and event pipeline

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| One session per community+viewer; dispose stops all work and writes | session.ts:203-218, 2650-2694 | DONE (session/mod.rs:174-263, 627-686) | - | - |
| Live events go through one ordered pipeline (discovery, read state, unread, outbox echo, profiles, emoji, timelines) | session.ts:517-578 | DONE (session/mod.rs:1150-1204) | - | - |
| Finite reads (head and older pages) go through the same pipeline as live events | session.ts:417-516 | DONE (session/channels.rs `admit_page`) | - | - |
| Visibility gate for auxiliary events (5/7/9005/40003/39005 need a known visible target); re-filter retained events on access change | event-access.ts:7-43; store.ts:2256-2335 | DONE (session/mod.rs `visible`, `revoke_access`; session/channels.rs `purge_retained`) | - | - |
| 401/403 on a channel read denies that channel | store.ts:476-511, 662-669 | DONE (session/mod.rs:1333-1341) | Disk-head removal comes with persistence | - |
| One send clock per session shared by outbox and projections | message-order.ts:21-58; store.ts:146 | DONE (session/mod.rs:607-609, 857-866) | - | - |

### Finite reader

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Shared identical reads, per-consumer cancel, 3 slots / 1 background, 10 s deadline, 128 pending, 1–4 filters, limit 1–500, 64 KiB key, 8 MiB response | reader.ts:46-299 | DONE (reader.rs:22-26, 131-560) | - | - |
| `invalidate()` on disconnect, access revocation, cache clear and before catch-up; `promote()` for selected channel catch-up | reader.ts:311-327; session.ts:371-380, 2253, 2291, 2484 | DONE (session/mod.rs, session/channels.rs) | - | - |
| Strict event checks, signature verification, 2048-entry proof memo, one bad event rejects the batch | events.ts:299-380 | DONE (event.rs:205-290) | - | - |

### Timeline windows: head and older pages

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Page filter: kinds 9/40002/40008/40099, `top_level`, `include_aux`, `include_summaries`, limit 20, `until`/`before_id` | window.ts:7-24 | DONE (window.rs:12, 33-50) | - | - |
| Signed 39006 bound checks | window.ts:27-80 | DONE (window.rs:53-124) | - | - |
| Head page replaces retained pages, older page appends; `hasMore = has_more && cursor` | store.ts:610-661 | DONE (session/timeline.rs:139-173) | - | - |
| Older page over budget (2400 rows / 8 MiB) discarded and sets `historyLimited` | store.ts:638-648 | DONE (session/timeline.rs `apply_older`) | - | - |
| Load-older guard | store.ts:2054-2072 | DONE (session/mod.rs:1259-1273) | - | - |
| Errors: first head fails = error; failure with rows keeps rows; "Retry older messages" | store.ts:662-680; ChannelTimeline.tsx:740-758 | DONE (timeline.rs:299-334; crates/app/src/ui/channel.rs:553-575) | - | - |
| Head newer than 60 s (`FRESH_FOR`) reused without a read | store.ts:99, 1981-1992 | DONE (timeline.rs:16, 133-136) | - | - |
| Revisit revalidates only while the reader is at the head; a paged-back reader is never replaced | store.ts:1966-1978 | DONE (session/channels.rs `ensure`) | - | - |
| Session-type channels: flat paging (20 rows, `#e` overlay paging 500/page up to 2000 events / 4 MiB, keep paging until empty) and replies kept as rows | session-window.ts:26-92; store.ts:117-122, 512-536 | DONE (session/channels.rs `read_session_window`; session/timeline.rs) | - | - |
| Prepared head cache (64 heads / 4 MiB) and hover/intent prepare | store.ts:179, 537-609, 1994-2040 | DONE (session/channels.rs; `Session::prepare`) | - | - |
| Window LRU of 3; mounted readers never evicted | store.ts:137, 356-364 | DONE (session/mod.rs:63, 1226-1242) | - | - |
| Eviction fences in-flight reads | store.ts:348-355, 398-402 | DONE (session/channels.rs `touch`, session-wide generation) | - | - |

### Live merge and window limits

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Live kinds accepted into an open window incl. transitive references | store.ts:2170-2199 | DONE (timeline.rs:18-29, 193-225) | - | - |
| Live events never move cursors | store.ts:2134 | DONE (timeline.rs:193-225) | - | - |
| Live overflow trimming (newest 2400 rows + local ops, shed oldest quarter under 8 MiB, set `historyLimited`) | store.ts:2204-2240 | DONE (session/timeline.rs `trim`) | - | - |
| Local outbox ops shown in the window; failed edits/reactions hidden, failed content kept | store.ts:267-303, 2080-2133 | DONE (session/channels.rs `touch`; session/mod.rs `local_operations`) | - | - |
| Tails: newest 256 live events per unopened channel (64 channels / 4 MiB), seeding windows and channel preview | store.ts:181-184, 215-266, 2145-2169 | DONE (session/channels.rs; `ChannelSummary::preview`) | - | - |
| Row profile demand incl. membership actor/target, workflow owner, mention references | membership.ts:39-49; store.ts:426-440 | DONE (session/timeline.rs `rows_people`) | - | - |

### Gap fill after reconnect

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| On disconnect mark heads/windows cached and cancel pending reads | store.ts:2384-2389; session.ts:2461-2486 | DONE (session/channels.rs `channels_disconnected`) | - | - |
| Catch-up head per retained window after route established, merged not replaced | session.ts:2527-2551; store.ts:2392-2432 | DONE (session/mod.rs:1084-1100; timeline.rs:177-188) | - | - |
| Pre-establishment head read cancelled so catch-up is a fresh post-subscribe read | session.ts:2228-2237, 2284-2292 | DONE (session/channels.rs `catch_up_next`) | - | - |
| Catch-up scheduling: max 2 concurrent, selected first, promotion, Retry-After pacing | session.ts:2238-2318 | DONE (session/channels.rs) | - | - |
| Per-channel catch-up state shown in "Live updates need attention" toast with Retry | session.ts:893-950; src/bundled/channels/LiveStatus.tsx:6-98 | DONE (session/mod.rs `LiveView::heads`; app ui/live_status.rs) | - | - |
| Older pages load over HTTP while socket is down | ChannelTimeline.tsx:637-646 | DONE (session/mod.rs:1259-1273) | - | - |

### Folding and ordering

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Row selection (row kinds, `h`, replies only with `broadcast=1`) | fold.ts:285-296 | DONE (fold/mod.rs:318-332) | - | - |
| Delete rules; deleted rows dropped | fold.ts:221-243, 296 | DONE (fold/mod.rs:203-224) | - | - |
| Edits by author/displayed author, latest wins, 40002 unwrap, `edited` | fold.ts:318-328 | DONE (fold/mod.rs:342-360) | - | - |
| Attachments/emoji from newest edit, mentions from original, `attachmentSourceId` | fold.ts:329-419 | DONE (fold/mod.rs:362-425) | - | - |
| Reaction grouping by (content, emoji URL), first-reaction order | fold.ts:432-487 | DONE (fold/mod.rs:470-517) | - | - |
| Thread summary 39005 tie-break | fold.ts:167-197, 261-266 | DONE (fold/mod.rs: the reference's lower-id tie-break) | - | - |
| 40099 membership rows | membership.ts:12-37; fold.ts:297-317 | DONE (fold/membership.rs:26-52) | - | - |
| Displayed author (`actor` / leading `p`), signer, workflow owner | fold.ts:199-219 | DONE (fold/mod.rs:175-195, 551-573) | - | - |
| Diff metadata, sent-from-thread excerpt, agent envelope, `mentionReferences` | fold.ts:352-411 | DONE (fold/mod.rs:401-440) | - | - |
| Rendered order (effective ms then id; `ms` tag) | message-order.ts:7-25 | DONE (fold/order.rs:19-43) | - | - |
| Send clock (5 s lead others, 60 s own) | message-order.ts:27-58 | DONE (fold/order.rs:47-95) | - | - |
| Incremental refold | message-projection.ts:13-169 | DONE (fold/projection.rs) | - | - |
| Unchanged rows keep identity | row-identity.ts:1-102 | DONE (fold/projection.rs) | - | - |
| `newer()` tie-break | events.ts:141-150 | DONE (event.rs:199-202) | - | - |

### Message content parsing

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `imeta` parsing (dim, blurhash, size, duration, waveform, voice note, filename, hash) | fold.ts:25-165 | DONE (fold/attachments.rs:93-176) | - | - |
| Attachments HTTPS-only | message-content.ts:21-24; fold.ts:98-99 | DIFFERENT | HTTP URLs kept as an "insecure" unavailable file card (fold/attachments.rs:81-88, 115-122, 160-163); looks deliberate | S |
| Markdown attachment projection | message-content.ts:113-223 | DONE (fold/attachments.rs:418-462, 593+) | - | - |
| Credential-free http(s) links; 100k length, depth 100 | message-content.ts:3-17, 67-99 | DONE (fold/attachments.rs:315-327, 465-480) | - | - |

### Message writes (`messages.ts`)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Send and reply tags (`h`, ≤32 `p`, `mention`, `emoji`, `imeta`, NIP-10) | messages.ts:75-99, 179-207 | DONE (messages.rs:462-517; session/mod.rs:1371-1406) | - | - |
| Every write kind requires "can participate" | messages.ts:42-48 | DONE (session/mod.rs `write`) | - | - |
| Edit copies `imeta` from folded `attachmentSourceId` | messages.ts:209-244 | DONE (session/mod.rs `write`; session/timeline.rs `row`) | - | - |
| React (custom emoji tag only for `:code:`) | messages.ts:245-264 | DONE (messages.rs:724-760) | - | - |
| Remove own messages/reactions with `k`; agent owner removes agent's messages | messages.ts:265-314 | DONE (session/actions.rs `removal_agent`; session/mod.rs) | - | - |
| Report (1984, NIP-56) | messages.ts:316-335 | DONE (`Session::report`, session/actions.rs) | - | - |
| Send thread reply to channel (sent-from-thread copy) | messages.ts:101-178 | DONE (`Session::send_to_channel`, session/mod.rs) | - | - |

### Persistence, caching and partitions

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Partition key `origin:viewer` | partition.ts:1-33 | DONE (store.rs:12-27) | - | - |
| Leaving purges heads, read state and outbox for that partition | partition-purge.ts:9-28 | DONE (store.rs:103-111, 397-410; crates/app/src/community.rs:211-240) | - | - |
| Disk head cache (signed events + author profiles; 64 heads / 8 MiB; 24 h; sessions excluded) | persistence.ts:31-204; store.ts:450-475, 596-606 | DONE (store.rs; session/channels.rs) | - | - |
| Startup restore of cached heads (last-selected first, re-verify, shown cached, revalidate) | store.ts:685-822, 1936-1993 | DONE (session/channels.rs `restore`, `hydrate`) | - | - |
| Disk-head retention follows roster | store.ts:950-962, 2332 | DONE (session/mod.rs `scan_done`; session/channels.rs) | - | - |
| Startup roster cache (≤2048 rosters + ≤1024 profiles, 24 h, author must match) | store.ts:823-903 | DONE (store.rs; session/channels.rs `save_startup`, `restore`) | - | - |
| Offline resume: cached-only session shows saved sidebar and heads ("Reconnect to update saved conversations.") | src/features/relay/service.ts:85-130; session.ts:417-431 | DONE (session/channels.rs `restored`; `SessionSnapshot::cached`) | - | - |
| Clear cache (Developer settings) and wipe disk heads when connection denied | session.ts:2604-2648; store.ts:1637-1659; service.ts:262-330 | DONE (`Session::clear_cache`; session/mod.rs `connected`) | - | - |

### Other

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Incoming live messages for notifications (first sight, single `h`, 4096-char preview) | incoming.ts:1-19; session.ts:2324-2448 | DONE (session/incoming.rs:1-203) | - | - |
| Delivery receipt read (16 KiB cap, strict UTF-8) | receipt.ts:1-21 | DONE (outbox.rs:53, 270-282) | - | - |
| Generic filtered views for plugins (`projectEvents`, `observe`, `subscribeLive`) | projection.ts:1-104; session.ts:1616-1622, 1924+ | DIFFERENT (kept): bundled plugins are compiled in and read the session through its own methods | - | M |
| Preload avatar media for the channel about to open (last 12 authors) | store.ts:403-425 | DONE (media.rs `prefetch_avatars`; session/channels.rs) | - | - |

## Outbox, publishing, attachments and media

The outbox engine is a close port. Main gaps: unsigned sends not shown in timelines and the outbox only existing after connect. The outbox panel, background uploads, ffmpeg media preparation and streaming playback are done. Paths: buzz-app `src/features/relay/`, `src/bundled/channels/` or `src-tauri/src/relay/`; port `crates/relay/src/` or `crates/app/src/ui/`.

Key findings:
1. Unsigned sends never show in the timeline: only signed ops reach timelines (session/mod.rs:1462-1468, `op.signed.clone()?`). A send queued behind 3 deliveries, failed before signing, or restored unsigned never appears and, with no outbox panel, cannot be retried or dismissed.
2. The outbox is created only after NIP-11 succeeds (session/mod.rs:846-888); before that every write fails with "Not connected to the relay yet" and restored ops are not shown (buzz-app creates it with the session, session.ts:264-317).
3. Done: the outbox panel is in channel settings' Diagnostics, and failed edits, deletes and reactions show their own status and retry.

### Outbox core

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Delivery states and record shape (recovery, acknowledged, guarded) | outbox.ts:17-43 | DONE (outbox.rs:81-133) | - | - |
| Event built at send time: `client-id` UUID, `ms` tag, local id | outbox.ts:679-744 | DONE (outbox.rs:775-827) | - | - |
| Send checks and texts (kind allowed, kind 9 non-empty, 32 KiB, MAX_PENDING 256, unique recovery key) | outbox.ts:647-678 | DONE (outbox.rs:724-757, 809-826) | - | - |
| Edit (40003) created_at +1 s per target, 60 s lead cap | outbox.ts:686-709 | DONE (outbox.rs:770-803) | - | - |
| NIP-AP catalog kinds: `supersedes`, head+1, larger limits | outbox.ts:665-674, 710-727 | DONE (crates/relay/src/outbox.rs: `SendOptions::supersedes`, max(now, head + 1, retained + 1); crates/relay/src/session/catalog.rs) | - | S |
| Unsigned intent saved before signing; signed bytes before publishing | outbox.ts:442-468 | DONE (outbox.rs:1358-1411) | - | - |
| ≤3 concurrent deliveries; 10 s deadline from enqueue | outbox.ts:90, 407-428 | DONE (outbox.rs:54-55, 1262-1337) | - | - |
| Failure classes, no downgrade, "Retry blocked/failed:" prefixes, timeout text | outbox.ts:381-406, 500-532 | DONE (outbox.rs:1472-1526) | - | - |
| acceptPublish status mapping, 16 KiB receipt | transport.ts:1313-1359 | DONE (outbox.rs:239-296) | - | - |
| Quota refusal (`sent:false` + `rate-limited:` or `quota:"api"` = rejected) | transport.ts:1326-1339 | DONE (outbox.rs; http.rs) | - | - |
| `accepted:false` with "duplicate:" | transport.ts:1352-1357 (failed) | DIFFERENT | Counted as accepted (outbox.rs:292), on purpose per spec §9.3 #3 | - |
| Event older than 15 min: strong read by id instead of re-post | native.ts:835-858 | DONE (outbox.rs:298-314) | - | - |
| Echo polling at 0/500/1500/4000/10000 ms | session.ts:579-602; outbox.ts:531-532 | DONE (outbox.rs:56-62, 1562-1607) | - | - |
| `observe()` marks seen on verified echo | outbox.ts:838-892 | DONE (outbox.rs:1609-1651; session/mod.rs:1186-1187) | - | - |
| Retry reuses saved signature; refusal rules | outbox.ts:767-794 | DONE (outbox.rs:849-886) | - | - |
| dismiss / recover / acknowledge rules and texts | outbox.ts:587-636, 795-815, 117-122 | DONE (outbox.rs:891-1000, 1339-1353) | - | - |
| Workflow receipt callback | session.ts:293-294 | DONE (crates/relay/src/session/mod.rs `on_receipt: Some(self.workflows.receipt_listener())`) | - | S |
| preparePublish: fresh mention roster check, canvas/workflow validation, re-check at publish start | session.ts:296-315 | DONE (session/publisher.rs; canvas and workflow checks come with those features) | - | - |
| `observeSend` hook (mention-wake, unhide DM on outgoing send) | outbox.ts:48-50, 153-177 | DONE (outbox.rs `observe_send`; `Session::observe_send`; its consumers are agent mention-wake and local DM hides) | - | - |
| Serialized journal commits; confirmed LRU 2048 / 8 MiB; "Outbox storage is full" over 2 MiB | outbox.ts:205-276, 127 | DONE (outbox.rs:45-49, 1065-1153; store.rs:136, 512-660) | - | - |
| Hydration budgets, batches, checks, no automatic replay, load error blocks sends | outbox.ts:292-379 | DONE (outbox.rs:1170-1258) | - | - |
| Migration from localStorage / IndexedDB v1 journal | outbox-storage.ts:57-95 | DIFFERENT | Not needed (no earlier native journal) | - |
| purgeConfirmed on access revocation; dispose | outbox.ts:826-837, 893-913 | DONE (outbox.rs:1011-1036) | - | - |
| Journal deleted on leave | outbox-storage.ts:27-49 | DONE (store.rs:397-409; crates/app/src/community.rs:237) | - | - |
| Outbox exists from session start (offline queueing, restored ops visible) | session.ts:264-317 | DONE (session/mod.rs: as in buzz-app, the outbox comes with the connected transport, and a saved snapshot has none) | - | - |
| Optimistic row shown at once with `sending` | outbox.ts:747-758 | DONE (event.rs `Verified::local`; session/mod.rs `local_operations`) | - | - |

### Outbox and delivery UI

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Outbox panel ("Outbox · N items", Sending/Sent/Unconfirmed/Not sent, 100-char excerpt, "Add agent …", note, RelayTimings) | src/bundled/channels/OutboxStatus.tsx:10-50, 98-99; ChannelsPage.tsx:1196-1200 | DONE (crates/app/src/ui/outbox_status.rs; crates/app/src/ui/channel_settings.rs Diagnostics) | The panel sits in channel settings' Diagnostics. RelayTimings is left out: the port has no profiler sample ring to capture (see "Relay profiler"). | - |
| Panel Retry (hidden for workflow/guarded) and guarded hint | OutboxStatus.tsx:52-69 | DONE (crates/app/src/ui/outbox_status.rs) | - | - |
| "Remove from outbox" with error alert | OutboxStatus.tsx:69-97 | DONE (crates/app/src/ui/outbox_status.rs) | Snapshot-share and session-link receipts are not acknowledged first: the port sends neither. | - |
| Message delivery notice: 10 s grace, "Couldn't send this message." / "Delivery not yet confirmed.", Retry | src/features/messages/delivery.ts:3-18 | DONE (crates/app/src/ui/message.rs:27, 1704-1722) | - | - |
| Edit/delete/reaction failure UI ("Retry message update", "Discard failed update", "Couldn't update reaction.", "Retry reaction") | MessageManagement.tsx:484-551; ReactionTool.tsx:145-166 | DONE (crates/app/src/ui/message_actions.rs:981-1003; crates/app/src/ui/message.rs:1613-1620) | - | - |
| No edit/delete of unconfirmed rows | useMessageEdit.ts:13-38 | DONE (crates/app/src/ui/message.rs:462) | - | - |
| useComposerSent: reveal sent message; re-select exact-message session channel | src/bundled/channels/useComposerSent.ts:4-19; ChannelsPage.tsx:545-550 | DONE (crates/app/src/ui/channel.rs: sending returns to the tail, which shows the sent message; an exact message opens in a thread panel here, see agents.md §18) | - | S |

### Channel writes (src-tauri/src/relay/channel_writes.rs)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Route-checked 9002/9008/9021/9022/41012 shapes | channel_writes.rs:21-80, 118-177 | DIFFERENT | No untrusted renderer to guard; builders in lifecycle.rs:33-34, 490-513, 607-620 | - |

### Attachments, limits, imeta

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Per-type caps (GIF 10 / image 50 / video 500 / other 100 MiB), error text | attachment-limits.ts:1-10; attachments.ts:30-32 | DONE (messages.rs:277-287; upload.rs:20, 29-32; crates/app/src/ui/composer.rs:1062) | - | - |
| ≤10 files per message | src/features/messages/attachment-draft.ts:34, 189-191 | DONE (crates/app/src/ui/composer.rs:40, 1052-1055) | - | - |
| 1,000 MiB retained-draft cap | attachment-draft.ts:35, 196-205 | DONE (crates/app/src/ui/composer/attachments.rs:22-23, 153) | - | - |
| Upload result validation (MIME, hex, size, same-origin `/media/<sha>`); image type sniffing | attachments.ts:55-97; attachment-limits.ts:12-21 | DONE (upload.rs:60-72, 345-361; messages.rs:295-333) | - | - |
| Upload response mapping (401/403, 413, 429, 8192-byte cap, JSON `code`) | attachments.ts:220-281 | DONE (crates/relay/src/upload.rs:454) | - | - |
| imeta field order, filename cleaning, label escaping, inline types | attachments.ts:98-106, 284-320 | DONE (messages.rs:348-420) | - | - |
| Voice imeta `duration` + `waveform` | attachments.ts:312, 322-334 | DIFFERENT | Duration only (messages.rs:271-273, 403-408), deliberate per spec §9.3 #2 | - |
| Reading blurhash, `dim`, `thumb`/`image`, waveform; blurhash preview | blurhash.ts:1-15 | DONE (fold/attachments.rs:131-152, 273-312; crates/app/src/ui/media.rs:151-170) | - | - |

### Upload pipeline

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Blossom 24242 auth (t=upload, x=sha, server, +60 s, "Upload attachment") | src-tauri/src/relay.rs:820-855 | DONE (crates/identity/src/lib.rs:261-284; media.rs:215-221) | - | - |
| PUT /upload with Content-Type, X-SHA-256, 600 s timeout | relay.rs:1145-1189 | DONE (upload.rs:307-327; media.rs:66-71) | - | - |
| Body streamed from disk in 64 KiB chunks | relay.rs:1105-1143 | DONE (crates/relay/src/upload.rs:16, 335; buzz-desktop `process_spool`) | - | - |
| Upload progress shown on the draft chip | relay.rs:1124-1129; attachment-draft.ts:128-140 | DONE (crates/app/src/ui/composer.rs:3216-3271) | - | - |
| Upload timing: queue on attach, upload serially at Send, Send stays enabled | attachment-draft.ts:257-266 | DONE (crates/app/src/ui/composer/attachments.rs:592; crates/app/src/ui/composer.rs `Submission::attachments`) | - | - |
| Removing a file aborts its upload; leaving pauses ("Upload paused. Retry to continue.") | attachment-draft.ts:70-84, 267-271; relay.rs:857-926 | DONE (crates/app/src/ui/composer/attachments.rs:205-213, 369, 631; crates/relay/src/upload.rs `cancel`) | - | - |
| Spool temp dirs, 4 spool slots, 64 active uploads, "Uploads are busy" | upload_spool.rs:14-15, 149-339 | DONE (crates/relay/src/upload.rs:16, 53; buzz-desktop `Spools`/`Uploads`) | - | - |
| Retry restarts from scratch | attachment-draft.ts:272-276 | DONE (composer.rs:1284-1292) | - | - |
| Image metadata stripping (JPEG q0.95 + orientation, PNG re-encode, WebP orientation, 25 MP cap) | prepare-attachment.ts:25-101 | DONE (crates/relay/src/upload.rs:705) | - | - |

### Media preparation

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| HEIC/HEIF to JPEG via ffmpeg ≥ 8 | video-preparation.ts:54-67; media_preparation.rs:99-141, 245-300 | DONE (crates/relay/src/upload.rs:239, 255; buzz-desktop preparation) | - | - |
| Video to MP4 (libx264 crf 23, yuv420p, aac 128k, faststart, metadata stripped; MP4 too) | video-preparation.ts:4-51; attachments.ts:113-121; media_preparation.rs:54-79, 300-345 | DONE (crates/relay/src/upload.rs:240, 255; buzz-desktop preparation) | - | - |
| Preparation limits (2 concurrent, 60 s / 600 s, 50/500 MiB output, ffmpeg texts) | media_preparation.rs:11-49, 153-189 | DONE (buzz-desktop preparation, through crates/relay/src/upload.rs) | - | - |
| Voice note MP4 with black 16x16 H.264 + AAC, 24 kHz mono, ≤300 s | media_preparation.rs:65-72, 276-286; voice-media.ts:2-3 | DIFFERENT | Same output via GStreamer (crates/app/src/ui/media/recorder.rs:1-45, 244-350) | - |
| Recording validation; picked `voice-note-*` audio converted | voice-media.ts:9-24; video-preparation.ts:69-93 | DONE (crates/relay/src/upload.rs:241, 255) | - | - |

### Media download and streaming

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Signed media GET, same-origin path check, no redirects, `.thumb.jpg` for avatars | relay.rs:1641-1717 | DONE (media.rs:98-105, 126-211, 223-244) | - | - |
| Ranged reads through 1 MiB block cache (32 blocks) | media_blocks.rs:18-174 | DONE (crates/app/src/ui/media/engine.rs; crates/relay/src/media.rs stream_url) | - | - |
| Non-Range origins: video spooled to disk up to 500 MiB | media_spool.rs:1-47, 152-293 | DONE (crates/app/src/ui/media/engine.rs; crates/relay/src/media.rs stream_url) | - | - |
| Loopback HTTP server for media elements | media_stream.rs:1-200 | DONE (crates/app/src/ui/media/engine.rs plays its URLs) | - | - |
| Response type policy and cache headers | relay.rs:1729-1809 | DONE (players: buzz-desktop's listener; images: crates/app/src/ui/media.rs) | Images are sniffed and relay SVG is refused like the policy; their cache stays the in-memory LRU (64 MiB) standing in for `max-age=3600`. | - |
| Avatar prefetch/warm | media.ts:37-143 | DONE (crates/relay/src/session/channels.rs `preload_media`, crates/relay/src/media.rs `prefetch_avatars`) | - | S |
| media_download to Downloads, errors returned | relay.rs:1579-1601 | DONE (crates/app/src/ui/media.rs save; attachments.rs; review.rs) | - | - |
| Copy image to clipboard in viewer | relay.rs:1553-1575; ImageReviewStage.tsx:140-150, 392 | DONE (crates/app/src/ui/media/review.rs) | - | - |

## Read state and unread

The data layer is ported almost one to one. The gaps are in the session API and the UI. Prefixes: buzz-app `src/`, port `crates/relay/src/` or `crates/app/src/`.

### Wire format and codec

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Read-state event recognition (30078, one `d`, one `t=read-state`, 32-hex slot) and target-key validation | features/relay/read-state-model.ts:26-33, 59-74 | DONE (read_state.rs:197-210, 270-289) | - | - |
| v1 payload parsing (unknown `v` fails closed, client_id 1..64, ≤10k keys / 128 KiB, `ov_s/c/b` groups, `esc:` unescape) | read-state-model.ts:43-133 | DONE (read_state.rs:294-364) | - | - |
| Encode for publish (sorted/escaped keys, override registers, re-parse, ≤40 KiB) | read-state-model.ts:225-258 | DONE (read_state.rs:474-505) | - | - |
| Codec: NIP-44 v2 to self, tags exactly `[d,t]`, ±60 s skew, 96 KiB / 128 KiB decode limits | read-state-host.ts:23-35; read-state.ts:291-318 | DONE (read_state.rs:523-578) | - | - |
| Post-sign decode confirms payload, coordinate, created_at | read-state.ts:555-567 | DONE (read_state.rs) | - | - |
| Event ≤64 KiB and decode re-check before `POST /events` | read-state-model.ts:6; src-tauri/src/relay.rs:548-562 | DONE (session/unread.rs `run_publish`) | - | - |
| Timestamp refusal classification (exact error + 400, or 503 `sent:false`) | read-state-host.ts:4-15; transport.ts:1289 | DONE (session/unread.rs `publish_outcome`) | - | - |

### Merge rules

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Max-per-key merge, 10k combined cap, `effectiveFrontier`, `overrideActive` | read-state-model.ts:144-190 | DONE (read_state.rs:368-417) | - | - |
| Publishing with overrides present | read-state.ts:531-533 (blocked: "Synchronized override publication is not enabled") | DIFFERENT | Kept on purpose (relay.md §9.3 #9): this client publishes frontiers only and keeps inherited floors, so a peer's override never stops read sync the way the reference's block does | S |

### Local journal

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Journal schema and validation; corrupt journal never overwritten; homes pruned; local = reserve ∪ sync | read-state-storage.ts:19-196; read-state.ts:84-91 | DONE (read_state.rs:1036-1216, 1564-1615) | - | - |
| Durable storage per partition; cross-window coordination | read-state-storage.ts:197-281; read-state.ts:56-66, 156-159, 253-263 | DONE (SQLite `read_state`, store.rs:745-797; single actor) | - | - |
| Purge partition on leave | read-state-storage.ts:213-230 | DONE (crates/app/src/community.rs:203-238; store.rs:397-410) | - | - |

### Retention and compaction

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `retainRead` (96 KiB, protected floors, 75% scoped share, recency, coverage pruning) | read-state-retention.ts:51-229 | DONE (read_state.rs:763-921) | - | - |
| `retainLocalRead` (5,000 keys / 512 KiB reserve) | read-state-retention.ts:244-325 | DONE (read_state.rs:936-1019) | - | - |
| Second retention at publish to 40 KiB | read-state.ts:534-540 | DONE (read_state.rs:2315-2322) | - | - |
| Coverage rules and `findHomes` (batches of 100, once per session) | unread.ts:590-624, 1557-1632 | DONE (unread.rs:976-1012, 2252-2316; session/unread.rs:649-676) | - | - |

### Sync lifecycle

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Capability/status/completeness snapshot; NIP-11 complete snapshot or bounded 500 read | read-state.ts:33-46, 146-155, 356-409; read-state-snapshot.ts:9-77 | DONE (read_state.rs:614-687, 1263-1300, 1857-1918; session/unread.rs:434-471) | - | - |
| Snapshot response capped at 8 MiB + 4 KiB | read-state-snapshot.ts:81-100 | DONE (session/unread.rs `read_snapshot`) | - | - |
| Read priorities: foreground on first ensure, background on reconnect | read-state.ts:702-703; unread.ts:1492, 2312-2314 | DONE (session/unread.rs) | - | - |
| Live read-state route, ingest newest per coordinate, 4096 slot cap | read-state.ts:319-355, 817-839; live.ts:600-608 | DONE (live.rs:493; session/mod.rs:767, 1185; session/unread.rs:386-395; read_state.rs:1769-1852) | - | - |
| Mutations with fences, revision, recent, localUnread | read-state.ts:410-490, 722-814 | DONE (read_state.rs:1922-2041) | - | - |
| 5000 ms non-resetting debounce; failed revision waits for retry | read-state.ts:113, 264-290, 575-590 | DONE (read_state.rs:51, 1727-1765, 2115-2130; session/unread.rs:550-585) | - | - |
| Flush: retry saved pending first, fresh own-slot read, slot conflict, save signed bytes before send, 10 s timeouts | read-state.ts:491-592 | DONE (read_state.rs:509-515, 2075-2349; session/unread.rs:43, 495-521) | - | - |
| `publishPending` readback and once-per-flush timestamp renewal | read-state.ts:593-670 | DONE (read_state.rs:2133-2288) | - | - |
| `stale()` on disconnect; refresh then repair on reconnect | read-state.ts:840-843; unread.ts:2312-2321 | DONE (session/unread.rs:296-326) | - | - |
| `retrySync()` / `refresh()` user actions | unread.ts:1838-1847 | DONE (`Session::retry_read_sync`, `Session::refresh_unread`) | - | - |

### Unread computation

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Evidence store (kinds, 4096 / 8 MiB, overflow clears and marks stale, depth-32 ownership) | unread.ts:153-186, 261-269, 1665-1706 | DONE (unread.rs:273-315, 485-488, 2019-2062) | - | - |
| Index: tombstones, own/joined/ownThreads, root walk | unread.ts:390-455 | DONE (unread.rs:1182-1258) | - | - |
| `mentionsViewer`, relevance/category/priority predicates, follows | unread.ts:372-389, 487-561, 625-642 | DONE (unread.rs:1140-1154, 1267-1356) | - | - |
| `isUnread` / `afterFrontier` rules | unread.ts:513-556 | DONE (unread.rs:1360-1407) | - | - |
| Per-target snapshot (null counts without evidence, latestMessage, manual, freshness) | unread.ts:693-766 | DONE (unread.rs:1476-1574) | - | - |
| Snapshot selector cache of 4096 | unread.ts:886-903 | DONE (unread.rs: as in the reference, which drops every entry without a listener when full; this port has no listeners) | - | - |
| DM vs mention classification ("direct wins in DMs") | unread.ts:625-639 | DONE (unread.rs: direct wins in DMs, so the tray no longer counts DMs twice) | - | - |
| Thread-activity projection (unread replies by root, preview with edits) | unread.ts:775-854 | DONE (unread.rs folded previews; `Session::thread_activity`; `ChannelUnread::threads`) | - | - |
| Channel-list changes republish/purge/repair | unread.ts:1108-1209 | DONE (unread.rs:2067-2143) | - | - |
| Evidence repair (128 channels per read, limit 500, progressive) | unread.ts:1492-1556 | DONE (unread.rs:2190-2248; session/unread.rs:591-660) | - | - |
| Mute does not change unread | docs §5i.7 | DONE (unread.rs:17-20) | - | - |
| Explicit follow / unfollow | unread.ts:1849-1876 | DONE (`Session::follow_thread`, `Session::thread_following`) | - | - |

### Lookups and attention

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Conversation-membership lookups (chunks of 50, reply pages, backoff `min(60 s, 1 s·2^n)`) | unread.ts:1218-1491, 1636-1664 | DONE (unread.rs:1410-1472, 1968-2013, 2326-2525; session/unread.rs:680-789) | - | - |
| `attention()` incl. `pending`; notifications use it | unread.ts:643-692 | DONE (unread.rs:1618-1686; session/incoming.rs:131, 175) | - | - |

### Automatic reading

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Reading lease (≤64, fenced) | unread.ts:1877-1899 | DONE (unread.rs:2962-3001) | - | - |
| `observe()` writes `msg:` marks; DM observation reads whole DM | unread.ts:1905-1938, 2021-2066 | DONE (unread.rs:3028-3069, 3163-3205) | - | - |
| Channel `catchUp()` | unread.ts:1956-2020 | DONE (unread.rs:3075-3159; session/unread.rs:905-928) | - | - |
| Thread-panel reading (`thread-activity:` catch-up, reply observations) | unread.ts:1958-1969; use-reading.ts | DONE (crates/app/src/ui/thread.rs, crates/relay/src/session/unread.rs) | `fire_dwell` only calls `catch_up(.., None)` (session/unread.rs:916-921); thread.rs reports no viewport | M |
| Dwell gating (fully visible rows, window focused, document visible, 300 ms, bottom within 1 px) | docs §5i.10; use-reading.ts:29-234 | DONE (crates/app/src/ui/reading.rs, crates/relay/src/session/unread.rs) | 300 ms matches (session/unread.rs:45) but partially visible rows count, only on scroll (crates/app/src/ui/channel.rs:164-179), no focus/visibility gating; new channel assumed at bottom (session/unread.rs:846-851) | M |

### Explicit read and unread actions

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `markChannelRead` | unread.ts:1772-1805, 2172-2178 | DONE (unread.rs:2691-2764; session/mod.rs:440-442) | - | - |
| `markAllChannelsRead` | unread.ts:2179-2233 | DONE (unread.rs:2770-2820; session/mod.rs:445-447) | - | - |
| `markThrough(target, msg)` | unread.ts:2069-2083 | DONE (crates/relay/src/session/unread.rs) | Engine done (unread.rs:2656-2670); not in session `Mark` (session/unread.rs:90-95) | S |
| `markMessageRead` | unread.ts:2110-2146 | DONE (unread.rs:2824-2857; session/mod.rs:450-452) | - | - |
| `markMessageUnread` and message menu "Mark unread" | unread.ts:2084-2109 | DONE (unread.rs:2862-2881; crates/app/src/ui/message.rs:485-491; crates/app/src/ui/channel.rs:428-437) | - | - |
| Message menu toggles "Mark unread" / "Mark read" | src/features/messages/MessageManagement.tsx:252-253 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs) | Always "Mark unread"; failures only logged (channel.rs:432-434) | S |
| `markUnreadLocal` / `clearUnreadLocal` | unread.ts:2234-2267 | DONE (crates/relay/src/session/unread.rs): Mark unread on this device; Mark as Read clears it, and nothing in buzz-app calls clearUnreadLocal. | Engine done (unread.rs:2902-2924); no session API | S |
| `enterChannel` / `leaveChannel` visit semantics | unread.ts:243-258, 1763-1770, 2147-2171 | DONE (unread.rs:2672-2689, 2927-2957; session/unread.rs:832-845, 915) | - | - |

### Unread UI

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Bold label for ordinary backlog | src/bundled/channels/UnreadBadge.tsx:50-56 | DIFFERENT (unread.rs:555-563; crates/app/src/ui/sidebar/rows.rs: the label is bold and, as Discord marks unread, a pill shows half out of the panel's left edge; the user asked for it) | - | - |
| Priority accent dot, no numerals | UnreadBadge.tsx:57-60, 80-87; docs/port/ui.md:967-969 | DONE (crates/app/src/ui/unread_badge.rs) | Numeric mention pill instead (crates/app/src/ui/sidebar.rs:168-184), which ui.md:969 rules out; replace with 6 px dot | S |
| Thread-activity dot "N unread threads" | UnreadBadge.tsx:57, 63-64, 88-95 | DONE (crates/app/src/ui/unread_badge.rs, crates/relay/src/session/unread.rs) | Needs activity exposure | S |
| Accessible/tooltip labels ("…Not an exact total", "Marked unread on this device only", "; may be out of date") | UnreadBadge.tsx:61-64 | DONE (crates/app/src/ui/unread_badge.rs) | Labels | S |
| Off-screen "N unread" edge pills with DM avatar stack; reveal expands section, centres and focuses row | src/bundled/channels/SidebarUnread.tsx:30-239 | DONE (crates/app/src/ui/sidebar_unread.rs): there are no collapsible sections to expand yet. | Implement | M |
| "Mark as Read" / "Mark as Unread (this device)" in sidebar and header menus, pending/"Saving…"/error | src/bundled/channels/ChannelReadMenuItem.tsx:24-56; useChannelReadAction.ts:1-32 | DONE (crates/app/src/ui/sidebar/menus.rs, crates/app/src/ui/sidebar.rs) | Menu items + action state (needs `markUnreadLocal`) | M |
| "Mark all as read" in community rail menu with disabled reasons and toasts | src/features/communities/CommunityRailItem.tsx:280-315 | DONE (crates/app/src/ui/rail_menu.rs) | - | S |
| Channel activity popover (hover 250/150 ms; "Working now" with timers; "Unread threads" rows with per-row "Mark as read"; stale note) | src/bundled/channels/ChannelActivityPopover.tsx:33-357 | DONE (crates/app/src/ui/channel_activity.rs): working rows show no timer, as the port captures no agent turns. | Needs `activity()` and `markThrough` in session | L |
| `UnreadOptions` panel (mark unread on device, mark read through loaded, sync status, Refresh, Retry read sync) | UnreadBadge.tsx:100-177 | DONE (crates/app/src/ui/channel_diagnostics.rs, in channel settings → Diagnostics) | - | M |
| `channel-activity.ts` values for Recent sort (kinds incl. 45001/45003, limit:1 per channel in batches of 128) | src/features/relay/channel-activity.ts:1-145 | DONE (crates/app/src/ui/sidebar.rs: the sidebar turns `Session::track_channel_activity` on while a section sorts by Recent and sorts by `ChannelSummary::last_activity_at`). Not done: the "Couldn’t refresh recent activity" notice and the startup wait for activity, as the session does not publish the activity read's status | - | M |

## Sidebar, channel discovery, navigation and DMs

Port data layer for the sidebar (`crates/relay/src/sidebar.rs`) is a near-complete pure port of the preference registers, edits, decoding and section projection, but nothing outside its own tests calls it. The UI sidebar (`crates/app/src/ui/sidebar.rs:76-284`) is a fixed Channels / Forums / Direct messages list.

### Discovery and membership (data)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Accept only relay-signed 39000/39002, capacity 1024 with overflow revision | src/features/relay/discovery.ts:40-80 | DONE (crates/relay/src/discovery.rs:317-389) | - | - |
| Deny / suspend / resume read-only previews after CLOSED | src/features/relay/discovery.ts:81-105 | DONE (crates/relay/src/discovery.rs:444-478) | - | - |
| Complete roster read proves absence (`retain`), `denyAll`, `restrictToKnown` for restored snapshots | src/features/relay/discovery.ts:110-143 | DONE (crates/relay/src/discovery.rs:494-540) | - | - |
| canAccess / canParticipate / authorized / open public metadata rules | src/features/relay/discovery.ts:145-171 | DONE (crates/relay/src/discovery.rs:541-578) | - | - |
| ChannelSummary projection (readOnly, cached, visibility, members, hidden, private, type incl. session + parentChannelId, archived, DM participants) | src/features/relay/discovery.ts:200-270 | DONE (crates/relay/src/discovery.rs:635-679) | - | - |
| clearCached and savedEvents for the roster snapshot | src/features/relay/discovery.ts:271-286 | DONE (crates/relay/src/discovery.rs:680-706) | - | - |
| Joined channels sorted by name then id | src/features/relay/discovery.ts:287-295 | DONE (crates/relay/src/discovery.rs:707) | - | - |
| 40099 membership activity parsing (relay-authored only, member_left/joined/removed) | src/features/relay/membership.ts:13-37 | DONE (crates/relay/src/discovery.rs:997) | - | - |
| `useListedChannel` per-channel fact selector | src/features/relay/listed-channel.ts:9-31 | DONE (snapshot `channels` read directly, crates/app/src/ui/sidebar.rs:83) | - | - |

### Sidebar sections and preferences

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Section layout: Starred, personal groups, Channels, Forums, Direct messages; empty Starred/Forums hidden | src/bundled/channels/sidebar-sections.ts:13-117 | DONE (crates/app/src/ui/sidebar.rs) | Load preferences and render through `sidebar_sections` | M |
| Per-section sort (A–Z / Recent by last activity) | src/bundled/channels/sidebar-sections.ts:40-55; src/bundled/channels/SidebarSection.tsx:160-195 | DONE (crates/app/src/ui/sidebar/menus.rs, crates/app/src/ui/sidebar.rs) | Section "More actions" menu with Sort submenu, write `channel-sort`, show sort errors | M |
| Read sidebar preferences (4 coordinates of kind 30078, foreground read, error prefixes "Preference query:" / "Preference decode:") | src/features/relay/sidebar-preferences.ts:199-238 | DONE (crates/relay/src/session/sidebar.rs) | Add a session-owned preferences store with ensure/refresh | M |
| Preference projection and validation budgets (100 sections, 1000 assignments, 500 stars/mutes, 104 sort keys) | src/features/relay/sidebar-preferences.ts:78-197 | DONE (crates/relay/src/sidebar.rs:1478-1534) | - | - |
| LWW register metadata (`meta`), legacy import, tombstones, 128 KiB budget, device id | src/features/relay/sidebar-registers.ts:1-300 | DONE (crates/relay/src/sidebar.rs:860-1310) | - | - |
| Pure edits: section removal, assignment, create-and-assign, sort, star/mute toggle | src/features/relay/sidebar-edits.ts:1-153 | DONE (crates/relay/src/sidebar.rs:1590-1760) | - | - |
| Native writer: strong head read, sign, publish, readback confirm, conflict error | src/features/relay/native-sidebar.ts:40-298 | DONE (crates/relay/src/session/sidebar.rs) | Session action to read head, sign 30078 (NIP-44 to self), publish, read back | M |
| Preferences store: serialized write queue, optimistic pending moves, failed-move retry/dismiss, sort failures, cached-state guards | src/features/relay/sidebar-preferences-store.ts:39-719 | DONE (crates/relay/src/sidebar_store.rs) | Port the store (pending/failed maps, generation, clear/dispose) | L |
| Star / unstar channel ("Move to… Starred") | src/features/channel-navigation/ChannelSidebar.tsx:583-592, 640-660 | DONE (crates/app/src/ui/sidebar/menus.rs) | Row menu item + star write | M |
| Move channel/conversation to a group, "Create new…", "Remove from <group>" | src/features/channel-navigation/ChannelSidebar.tsx:600-720 | DONE (crates/app/src/ui/sidebar/menus.rs) | Move submenu and assignment writes | M |
| Create new section dialog ("Create new section", "Section name") | src/bundled/channels/CreateSidebarSection.tsx:39-80 | DONE (crates/app/src/ui/sidebar/dialogs.rs) | Dialog that calls create-and-assign | S |
| Remove section with confirm ("…will move back to Channels…") | src/bundled/channels/SidebarSection.tsx:212-260 | DONE (crates/app/src/ui/sidebar/dialogs.rs) | Section menu item, confirm dialog, removal write | S |
| Collapse / expand section, persisted per scope | src/bundled/channels/SidebarSection.tsx:126-210; src/bundled/channels/useSidebarView.ts:15-47 | DONE (crates/app/src/ui/sidebar.rs, crates/app/src/ui/sidebar/rows.rs) | Toggle state saved in view state | S |
| Drag and drop channels between sections | src/features/channel-navigation/ChannelSidebarDnd.tsx:1-117 | DONE (crates/app/src/ui/sidebar/rows.rs; GPUI starts a drag after 2px, not 6px) | Drag sources and drop targets per section key | M |
| Section group icons (emoji / custom emoji) | src/bundled/channels/SidebarGroupIcon.tsx:1-79 | DONE (crates/app/src/ui/sidebar/rows.rs, crates/app/src/ui/sidebar/menus.rs) | Render group icon in header and menus | S |
| Personal groups dialog ("Personal groups", "Group name", add/remove/save) | src/bundled/channels/ChannelGroupsDialog.tsx:13-237; src/features/relay/sidebar-personal-groups.ts:42-172 | DONE (crates/app/src/ui/settings/personal_groups.rs) | Dialog and removal/assignment flows with their Outbox-guidance error texts | M |
| Mute / Unmute with optimistic intent | src/features/channel-navigation/ChannelSidebar.tsx:728-744; src/bundled/channels/useOptimisticMute.ts:13-72 | DONE (crates/app/src/ui/sidebar.rs, crates/app/src/ui/sidebar/menus.rs) | Row/header menu item, mute write, optimistic state, muted unread suppression input | M |
| Sidebar row context menu (right click, ContextMenu key, Shift+F10) | src/bundled/channels/ChannelSidebarItem.tsx:272-344; src/bundled/channels/useChannelRowMenu.ts:1-41 | DONE (crates/app/src/ui/sidebar/rows.rs, crates/app/src/ui/sidebar/menus.rs) | Context menu with move, mute, read, copy link, lifecycle, remove-from-Messages | M |
| Row menu "Copy link" (`buzz://channel/<encoded id>`, toast "Link copied." / "Couldn't copy the link.") | src/bundled/channels/ChannelCopyLinkMenuItem.tsx:5-24 | DONE (crates/app/src/ui/sidebar/menus.rs; GPUI's clipboard write cannot fail, so there is no failure toast) | Add to row menu, encode id, show toasts | S |
| Row menu lifecycle actions | src/features/channel-navigation/ChannelSidebar.tsx:763-772 | DONE (crates/app/src/ui/sidebar/menus.rs, crates/app/src/ui/channel_settings.rs `LifecycleDialog`) | Reuse in row menu | S |
| Startup reveal latch (1500 ms budget, wait for prefs/names/activity/unread) and "updating" state | src/bundled/channels/useSidebarStartup.ts:11-87 | DONE (crates/app/src/ui/sidebar.rs; waits for the live socket rather than profile names) | Latch the first reveal | S |
| Channel sidebar width resize (220–520, default 260, double-click reset, persisted) | src/bundled/channels/useSidebarView.ts:4-13; src/features/channel-navigation/ChannelSidebarResizeHandle.tsx:1-27 | DONE (crates/app/src/ui/resize_handle.rs, crates/app/src/ui/workspace/shell.rs) | Resize handle and persisted width | S |
| Scroll position restore and reveal current row | src/bundled/channels/useSidebarView.ts:54-228 | DONE (crates/app/src/ui/sidebar.rs) | Save scroll per scope; scroll selected row into view | S |
| Sidebar connection states: "Connecting to your relay…", error + "Retry channels", "Choose a community to see channels.", "No channels yet.", "Retry connection", error boundary "Channels couldn't open." | src/features/channel-navigation/ChannelSidebar.tsx:100-185, 1229-1236 | DONE (crates/app/src/ui/sidebar.rs; no community or error boundary state applies) | Render status and retry | S |
| "Saving…" status and move error notes | src/features/channel-navigation/ChannelSidebar.tsx:1195-1210 | DONE (crates/app/src/ui/sidebar.rs, crates/app/src/ui/sidebar/menus.rs) | Comes with the preferences store | S |

### Sidebar rows

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Active filter: not archived, hidden only for DMs; DMs hidden by viewer excluded | src/bundled/channels/sidebar-sections.ts:18-28 | DONE (crates/app/src/ui/sidebar.rs:52-54, 83) | - | - |
| Channel icon: lock for private, hash otherwise; DM icon | src/features/channels/channel-icon.ts:4-8 | DONE (crates/app/src/ui/message.rs:1035-1042) | - | - |
| DM label from participant names, "Notes to self", label profile demand capped at 1024 | src/bundled/channels/useChannelLabels.ts:9-77 | DONE (crates/app/src/ui/sidebar.rs:37-50; crates/relay/src/session/mod.rs:1030) | - | - |
| 1:1 DM avatar with presence badge | src/bundled/channels/ChannelSidebarItem.tsx:186-200 | DONE (crates/app/src/ui/sidebar.rs:243-270) | - | - |
| Agent avatars use squircle shape | src/bundled/channels/ChannelSidebarItem.tsx:197 | DONE (crates/app/src/ui/sidebar/rows.rs) | Shape by `isAgent` | S |
| Group DM shows participant count badge ("N other participants") | src/bundled/channels/ChannelSidebarItem.tsx:202-212 | DONE (crates/app/src/ui/sidebar/rows.rs) | Count badge | S |
| User status shown next to 1:1 DM name (compact) | src/bundled/channels/ChannelSidebarItem.tsx:219-228 | DONE (crates/app/src/ui/sidebar/rows.rs; read in batches every 2 minutes, not live) | Render user status accessory | S |
| DM typing badge ("X is typing...", remote humans, incl. threads) | src/bundled/channels/DmTypingBadge.tsx:8-94 | DONE (crates/app/src/ui/sidebar/rows.rs) | Typing badge on the DM avatar | S |
| Working-agents indicator (turns + channel-level typing, label "<agents> working in <channel>") | src/bundled/channels/working-agents.ts:7-99; src/bundled/channels/ChannelSidebarItem.tsx:238-250 | DONE (crates/app/src/ui/sidebar/rows.rs with `agent_activity::with_observed`) | - | M |
| Sessions nested under parent channel with collapse, "New session" draft row | src/bundled/channels/ChannelSidebarRow.tsx:96-190; src/features/channel-navigation/ChannelNavigationState.tsx:100-185 | DONE (crates/app/src/ui/sidebar.rs; buzz-app's sidebar also renders them flat) | Nest by `parent_channel_id`, collapse state, drafts persisted as `sessions:channel-drafts` | M |
| DM "Remove from Messages" (X button and menu), local viewer-scoped hide with probes that restore on new messages | src/bundled/channels/useHiddenDms.ts:1-237; src/bundled/channels/ChannelSidebarRow.tsx:130-160 | DONE (crates/app/src/ui/sidebar/menus.rs, crates/relay/src/session/mod.rs) | Wire local hides, X button, focus move, probe loop | M |
| Header menu shares row actions (mute/read/copy link) via navigation state | src/features/channel-navigation/ChannelNavigationState.tsx:20-60 | DONE (crates/app/src/ui/sidebar/menus.rs `header_actions`, crates/app/src/ui/channel_menu.rs) | Shared action provider for header menu | S |

### Navigation and links

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Routes: `new-message`, `empty`, `{kind:"new-session", parentId}` | src/features/channel-navigation/routes.ts:3-25 | DONE (crates/app/src/ui/navigation.rs `ChannelRoute`, crates/app/src/ui/workspace/routes.rs) | Add new-session route | S |
| Prepare DM: select the new DM once discovery admits it | src/features/channel-navigation/ChannelNavigationState.tsx:111-130 | DONE (crates/app/src/ui/workspace.rs:721-757) | - | - |
| Public channel search for Cmd-K / `#` completion (180 ms debounce, keep still-matching results while loading, readOnly filter, error text "Public channel search couldn't finish…", retry, partial flag) | src/features/channels/usePublicChannelSearch.ts:26-96 | DONE (crates/app/src/ui/search.rs, crates/app/src/ui/composer/channels.rs, crates/relay/src/session/actions.rs `search_public_channel_page`) | Partial/err/retry handling | S |

### Direct messages

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Participant policy: 1–8 unique valid keys, not viewer; "Choose between one and eight other people." | src/features/relay/direct-message-policy.ts:2-12; src/features/relay/direct-messages.ts:66-70 | DONE (crates/relay/src/dm.rs; crates/app/src/ui/new_message.rs:22) | - | - |
| People search paging (15 first browse page, 30 otherwise, prefix search, exact key, viewer excluded, `hasMore`) | src/features/relay/direct-messages.ts:27-65 | DONE (crates/relay/src/dm.rs:56-100; crates/relay/src/session/actions.rs:213-224) | - | - |
| Open DM (41010), strong 39000/39002 confirm, participants check, unhide | src/features/relay/direct-messages.ts:66-102 | DONE (crates/relay/src/session/actions.rs:676-702) | - | - |
| `delivered(id, channelId)`: confirm an earlier first message from journal or relay read, retry failed/unknown, wait on outbox | src/features/relay/direct-messages.ts:103-184 | DONE (`Session::dm_delivered`; dm.rs `sent_filter`) | - | - |
| New message as a full page (not a dialog) with composer (mentions, attachments) | src/features/direct-messages/NewMessage.tsx:363-440 | DONE (crates/app/src/ui/new_message.rs, crates/app/src/ui/workspace/dialogs.rs) | - | M |
| Persist recipients and draft per scope; restore after restart ("Could not restore the message.") | src/features/direct-messages/NewMessage.tsx:20-110, 160-205 | DONE (crates/app/src/ui/new_message.rs) | - | S |
| Pending send recovery ("Retry the earlier message to confirm its delivery before editing.", retry button) | src/features/direct-messages/NewMessage.tsx:270-410 | DONE (crates/app/src/ui/new_message.rs: outbox recovery `direct-message:new`) | - | M |
| Selected agent no longer available guard | src/features/direct-messages/NewMessage.tsx:226-230 | DONE (crates/app/src/ui/new_message.rs `validate_agents`) | - | S |
| Recipient picker: chips, Backspace removes last, Escape, Enter/Tab, "Find a person…" / "Add another person…", "Agent" tag | src/features/direct-messages/RecipientPicker.tsx:290-500 | DONE (crates/app/src/ui/new_message.rs:120-150, 286, 330-336) | - | - |
| Picker "Load more people" / "Retry loading people" paging | src/features/direct-messages/RecipientPicker.tsx:505-540 | DONE (crates/app/src/ui/new_message.rs) | - | S |
| Picker name-continuation rule (waits while a longer name exists) | src/features/direct-messages/RecipientPicker.tsx:205-230 | DONE (crates/app/src/ui/new_message.rs `exact_recipient`) | - | S |
| Chip removal focus handling | src/features/direct-messages/useChipRemoval.tsx | DONE (crates/app/src/ui/new_message.rs: poof, plop and focus back to the To field) | - | S |

## Channel members, moderation and profiles

### Channel members (data)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `canAddMembers`: stream/forum, not archived, member or open public preview | src/features/channel-members/members.ts:19-29 | DONE (crates/relay/src/members.rs:44) | - | - |
| `canShareSession`: add to a private session only | src/features/channel-members/members.ts:33-47 | DONE (members.rs `can_share_session`; `Session::add_session_participant`) | - | - |
| Add member (9000, `role: bot` for agents) through outbox with recovery key `member-add:<channel>:<pubkey>` | src/features/channel-members/members.ts:90-222 | DONE (crates/relay/src/session/actions.rs:574-640; crates/relay/src/members.rs:55-80) | - | - |
| Session participants: agents rejected ("Add agents with the session agent picker instead.") | src/features/channel-members/members.ts:119-128 | DONE (session/members.rs `AddOps::check`) | - | - |
| Failed addition older than 15 min must be removed from Outbox first | src/features/channel-members/members.ts:180-186 | DONE (session/members.rs `AddOps::add`) | - | - |
| Guarded (agent-profile) invitation retry rule | src/features/channel-members/members.ts:161-164 | DONE (session/members.rs `AddOps::add`) | - | - |
| Start added managed agent after confirmed membership (`startAddedAgent`, MembershipChanged error) | src/features/channel-members/members.ts:224-294 | DONE (crates/app/src/ui/members.rs `start_added_agent`) | - | M |
| Member search (kind 0 prefix, 30 per page, npub/hex exact, rank by name tier, 256 cap) | src/features/channel-members/search.ts:9-58 | DONE (crates/relay/src/members.rs:218-340) | - | - |
| Administration authority: roles admin/member/guest/owner/bot, canManage/canRemove, authorize, confirmation | src/features/channel-members/administration-protocol.ts:10-207 | DONE (crates/relay/src/members.rs:84-190; crates/relay/src/lifecycle.rs) | - | - |
| Owner may remove own agent with fresh attestation | src/features/channel-members/administration.ts:200-240 | DONE (crates/relay/src/session/actions.rs:648-660) | - | - |
| Uncertain administration result: "This request may have taken effect… it will not be sent again." | src/features/channel-members/administration.ts:38 | DONE (session/members.rs; `Session::member_operation`, `Session::refresh_members`) | - | - |
| Single in-flight operation guard ("A member operation is still in progress.") | src/features/channel-members/administration.ts:101 | DONE (session/members.rs `Members::begin_operation`) | - | - |
| Addition receipt tracking across dialog reopen | src/features/channel-members/operations.ts:15-162 | DONE (session/members.rs `Members`, `track_additions`; `Session::member_additions`) | - | - |

### Channel members dialog

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Roster grouped Owners/Admins/Members/Agents with presence | src/bundled/channels/ChannelMembersDialog.tsx:632-660 | DONE (crates/app/src/ui/members.rs:30-60) | - | - |
| Role filter "All · N" select | src/bundled/channels/ChannelMembersDialog.tsx:1151-1170 | DONE (crates/app/src/ui/members.rs:591-640) | - | - |
| Search box "Add people and agents" / "Search members"; "Not in this channel" results with Add / "Adding…" | src/bundled/channels/ChannelMembersDialog.tsx:1105-1260 | DONE (crates/app/src/ui/members.rs:160-165, 515-525, 750-775) | - | - |
| Search results "Show more results" paging | src/bundled/channels/ChannelMembersDialog.tsx:1290-1302 | DONE (crates/app/src/ui/members.rs) | - | S |
| Disabled-add reasons (DM, archived, unavailable, join first) | src/bundled/channels/ChannelMembersDialog.tsx:1186-1194 | DONE (crates/app/src/ui/members.rs): a saved conversation shown offline gives the connection reason | - | S |
| Member row menu: View profile, View owner profile, Send message, Make admin/member, Remove | src/bundled/channels/MemberAdministration.tsx:230-300 | DONE (crates/app/src/ui/members.rs:340-400) | - | - |
| Role change and removal confirmations with exact copy | src/bundled/channels/ChannelMembersDialog.tsx:995-1100 | DONE (crates/app/src/ui/members.rs:550-590) | - | - |
| "managed by <owner> (you)" attribution | src/bundled/channels/ChannelMembersDialog.tsx:220-235; useMemberOwners.ts | DONE (crates/app/src/ui/members.rs:418-428) | - | - |
| Archived identities labelled/hidden; "Archived identities could not be checked." | src/bundled/channels/ChannelMembersDialog.tsx:205, 1318-1322 | DONE (crates/app/src/ui/members.rs, crates/app/src/ui/archives.rs) | - | S |
| "Role unverified" label | src/bundled/channels/ChannelMembersDialog.tsx:935 | DONE (crates/app/src/ui/members.rs: the row's accessible label, as in buzz-app) | - | S |
| Roster load errors ("The member list could not be confirmed." / "could not load."), names-failed note, owners-failed note, "Some agents could not load." | src/bundled/channels/ChannelMembersDialog.tsx:520-565, 1307-1318 | DONE (crates/app/src/ui/members.rs; crates/relay/src/session/actions.rs `channel_state`, `load_profiles`) | - | S |
| Per-person addition error with Retry / "Retrying…" | src/bundled/channels/ChannelMembersDialog.tsx:1324-1338 | DONE (crates/app/src/ui/members.rs, from `Session::member_additions`). A failed agent start is kept in the dialog, not the session, so it does not survive reopening | - | S |
| "<name> is in the channel." live announcement; "Opening conversation…" status | src/bundled/channels/ChannelMembersDialog.tsx:798, 1340-1344 | DONE (crates/app/src/ui/members.rs) | - | S |
| Refresh member data button | src/bundled/channels/ChannelMembersDialog.tsx:1074 | DONE (crates/app/src/ui/members.rs:789) | - | - |

### Community moderation (Settings → Membership)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Membership settings card visibility (manager / pending / error) | src/bundled/moderation/index.tsx:5-48 | DONE (crates/app/src/ui/moderation/mod.rs, crates/app/src/ui/settings/cards.rs) | - | M |
| NIP-43 roster (13534, relay-authored) read and role derivation | src/features/communities/roster.ts:1-62; src/bundled/moderation/membership.ts:27-150 | DONE (crates/relay/src/moderation.rs, crates/app/src/ui/moderation/store.rs) | - | M |
| Member list with search, Make admin / Make member / Remove with confirmations and copy | src/bundled/moderation/CommunityAdmin.tsx:250-420 | DONE (crates/app/src/ui/moderation/card.rs, crates/relay/src/session/moderation.rs) | - | L |
| Add by npub/hex with role, "already a community member" check | src/bundled/moderation/CommunityAdmin.tsx:425-575 | DONE (crates/app/src/ui/moderation/invite.rs) | - | M |
| Invite links: mint, copy, expiry and max-uses options, retry | src/bundled/moderation/CommunityAdmin.tsx:597-720; src/bundled/moderation/api.ts:37-50 | DONE (crates/app/src/ui/moderation/invite.rs, crates/relay/src/session/moderation.rs) | - | M |
| Stale-list guard "The member list is out of date. Retry before making changes." | src/bundled/moderation/CommunityAdmin.tsx:63 | DONE (crates/app/src/ui/moderation/card.rs, crates/app/src/ui/moderation/invite.rs) | - | S |

### Profiles

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Kind 0 fold (display_name/name, picture, agent auth tag, fallback name) | src/features/relay/profiles.ts:7-73 | DONE (crates/relay/src/profiles.rs:38-125) | - | - |
| Profile directory: batched 500-author reads, in-flight dedupe, 1024-entry LRU | src/features/relay/profile-directory.ts:25-160 | DONE (crates/relay/src/profiles.rs:13-15, 183-305) | - | - |
| Profile directory 2 MiB byte cap | src/features/relay/profile-directory.ts:25 | DONE (profiles.rs; budget.rs) | - | - |
| Profile selection (per-id stable snapshot) | src/features/relay/profile-selection.ts:1-34 | DONE (snapshot map, crates/relay/src/profiles.rs:183) | - | - |
| Profile panel (side panel, tabs Info / Runtime / Channels / Memories) | src/bundled/profiles/ProfilePanel.tsx:370-620 | DONE (crates/app/src/ui/profile_panel.rs): a tab beside the conversation for profile links, or the companion dock elsewhere. The agent-only sections are the rows below | - | L |
| Copy npub with status text | src/bundled/profiles/ProfilePanel.tsx:521-540 | DONE (crates/app/src/ui/profile_panel.rs) | - | S |
| Presence label (Active / Away / Offline) in profile | src/bundled/profiles/ProfilePanel.tsx:410-415 | DONE (crates/app/src/ui/profile_panel.rs) | - | S |
| Profile activity preview ("Latest activity", states) | src/bundled/profiles/ProfileActivity.tsx:44-70; activity-preview.ts | DONE (crates/app/src/ui/agent_activity/profile.rs) | - | M |
| Profile channels list ("No visible channels with verified membership…") | src/bundled/profiles/ProfileChannels.tsx:49-95 | DONE (crates/app/src/ui/profile_panel.rs). Not done: the partial-roster notes, as the session does not publish roster coverage | - | S |
| Add agent to channel from profile | src/bundled/profiles/ProfileAddChannel.tsx:90-205 | DONE (crates/app/src/ui/profile_panel.rs, crates/app/src/ui/members.rs `start_added_agent`) | - | M |
| Agent instances, runtime, actions, delete, archive, identity, harness log, memories, public metadata | src/bundled/profiles/ProfileInstances.tsx, ProfileRuntime.tsx, ProfileAgentActions.tsx, ProfileAgentDelete.tsx, ProfileAgentArchive.tsx, ProfileAgentIdentity.tsx, ProfileHarnessLog.tsx, ProfileMemories.tsx, ProfilePublicMetadata.tsx | DONE (crates/app/src/ui/profile_panel/agent.rs, crates/app/src/ui/profile_panel.rs) | - | L |
| Instance profile panel (`parseInstanceTarget`) | src/bundled/profiles/InstanceProfilePanel.tsx | DONE (crates/app/src/ui/profile_panel/agent.rs: `buzz:agent-instance:` links, and instances opening in place) | - | M |
| "Message" from profile opens DM ("Could not open the conversation. Try again.") | src/bundled/profiles/ProfilePanel.tsx:330-350 | DONE (crates/app/src/ui/profile_card.rs:45-55; crates/app/src/ui/workspace.rs:737) | - | - |

## Communities, hosted communities and inbox

### Communities

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Destination / relay origin normalization | src/features/communities/destination.ts:1-73 | DONE (crates/relay/src/origin.rs) | - | - |
| Saved communities per viewer, selected community persisted | src/features/communities/service.ts | DONE (crates/app/src/community.rs:49-170) | - | - |
| NIP-11 name ("Buzz Relay" ignored) | src/features/communities/service.ts; CommunityDialog.tsx:233 | DONE (crates/relay/src/community.rs:141-215) | - | - |
| Community icon from NIP-11, cached, fallback to initials | src/features/communities/community-icon.ts:5-68; CommunityRail.tsx:99-128 | DONE (crates/relay/src/community_icon.rs; crates/app/src/community.rs saves it; crates/app/src/ui/rail_menu.rs) | - | S |
| Invite links: `buzz://join`, share link `https://host/invite/<code>` | src/features/communities/invite-link.ts:1-47 | DONE (crates/relay/src/community.rs:55-130; crates/app/src/deep_links.rs:44) | - | - |
| OS invite intent (one pending request, cleared by request id) | src/features/communities/invite-intent.ts:4-33 | DONE (crates/app/src/deep_links.rs; crates/app/src/ui/add_community.rs) | - | - |
| Join journal (public progress only), resume | src/features/communities/join-journal.ts:1-89 | DONE (crates/relay/src/community.rs:342-420; crates/app/src/ui/add_community.rs:90-145) | - | - |
| Add community dialog: relay URL, invite code, policy (terms, age), profile step | src/features/communities/CommunityDialog.tsx:300-445 | DONE (crates/app/src/ui/add_community.rs:480-700) | - | - |
| Invite error copy (expired / exhausted / invalid / policy changed) | src/features/communities/CommunityDialog.tsx:24-33 | DONE (crates/relay/src/community.rs `claim_error`) | - | S |
| Profile default per community, ProfileFields | src/features/communities/profile-default.ts; ProfileFields.tsx:52-70 | DONE (crates/app/src/ui/add_community.rs:640-690) | - | - |
| Avatar URL field on join profile | (not in buzz-app ProfileFields) | DONE (crates/app/src/ui/add_community.rs: removed; a loaded picture is kept) | - | S |
| Leave community (28936), refusals already-absent / revoked, purge device state unless revoked | src/features/communities/leave-protocol.ts:1-59; service.ts:321-375; device-state.ts:64-83 | DONE (crates/relay/src/account.rs:18-280; crates/app/src/community.rs:199-245) | - | - |
| Purge failures reported by store name | src/features/communities/device-state.ts:41-83 | DONE (crates/app/src/community.rs `leave` residue; crates/app/src/ui/settings/communities.rs `left_text`) | - | S |
| Community rail: Personal space button | src/features/communities/CommunityRail.tsx:193-200 | DONE (crates/app/src/ui/rail_menu.rs, crates/app/src/community.rs `select_personal`) | - | M |
| Rail item context menu: Copy community URL, Invite to community, Community settings, Leave community ("Mark all as read" item: see read state) | src/features/communities/CommunityRailItem.tsx:120-310 | DONE (crates/app/src/ui/rail_menu.rs) | - | M |
| Rail tooltip / label "Switch to <name>" | src/features/communities/CommunityRailItem.tsx:159 | DONE (crates/app/src/ui/rail_menu.rs: the name as tooltip) | - | S |
| `useCommunityRole` (owner/admin from 13534) gating admin menu items | src/features/communities/useCommunityRole.ts:23-69 | DONE (crates/app/src/ui/moderation/mod.rs `manager`, `refresh_role`) | - | S |
| Admin protocol (invite request, member command kinds, reasons) | src/features/communities/admin-protocol.ts:11-108 | DONE (crates/relay/src/moderation.rs) | - | M |
| Connection status per community | src/features/communities/connection.ts:1-22 | DONE: not applicable. buzz-app's rail shows no connection state; `connection.ts` only registers and connects transports. Connection problems show in the live status toast (ui/live_status.rs) | - | S |

### Hosted communities

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Settings card "Hosted communities" (Builderlab sign-in, account, identity link/unpair, list, quota, create, delete with recovery) | src/bundled/hosted-communities/index.tsx:1-20; HostedCommunities.tsx:120-700; api.ts:1-110; native.ts:8-90 | SKIPPED (decided: Block-hosted services) | - | L |

### Inbox

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Inbox page (primary page) with connecting/error/retry states | src/bundled/inbox/index.tsx; InboxPage.tsx:82-115 | DONE (crates/app/src/ui/inbox.rs) | - | L |
| Inbox items computation (mentions, threads, DMs) | src/features/relay/inbox.ts:4-30 | DONE (crates/relay/src/session/inbox.rs `Session::inbox`) | - | S |
| Inbox history feed (mentions `#p` kinds 9/40002, 50 per page, cursor until/before_id, edit/delete completeness) | src/features/relay/inbox-feed.ts:1-253 | DONE (crates/relay/src/session/inbox.rs) | - | M |
| Filters (All / Threads / Mentions / DMs / Drafts; From People/Agents; Unread only) | src/bundled/inbox/InboxFilters.tsx:24-120 | DONE (crates/app/src/ui/inbox.rs, crates/app/src/ui/inbox/model.rs) | - | M |
| Inbox detail with channel preview and "Open in channel" | src/bundled/inbox/InboxDetail.tsx:160-240; ChannelPreview.tsx | DONE (crates/app/src/ui/inbox/detail.rs) | - | M |
| Drafts view (list, open in origin, delete with confirm) | src/bundled/inbox/DraftsView.tsx:160-440 | DONE (crates/app/src/ui/inbox/drafts.rs) | - | M |
| DM labels in inbox | src/bundled/inbox/dm-label.ts | DONE (crates/app/src/ui/inbox/model.rs `dm_label`) | - | S |
| Inbox actions expiry/error copy | src/bundled/inbox/InboxPage.tsx:370-385 | DONE (crates/app/src/ui/inbox.rs, crates/relay/src/session/inbox.rs) | - | S |

## Other relay-layer features

Emoji, GIFs, threads, thread windows and typing are close to one to one. Missing entirely: reminders, identity archives, product feedback, Me preferences (beyond a pure model), native agent commands and project Git. Missing wiring: other people's statuses, thread Follow/Unfollow, work-session creation. Paths: buzz-app `src/features/relay/` or `src-tauri/src/relay/`; port `crates/relay/src/`.

### Emoji

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Catalog read `{kinds:[30030],#d:[buzz:custom-emoji],limit:500}` background | emoji-directory.ts:111-150 | DONE (emoji.rs:79-84; session/mod.rs:1514-1531) | - | - |
| 500 sets / 2 MiB budget, over budget = error and empty palette | emoji-directory.ts:29-30, 58-83, 117-121 | DONE (emoji.rs:224-231, 254-286) | - | - |
| "may be incomplete" at ≥500 sets | emoji-directory.ts:134-137 | DONE (emoji.rs:234-250) | - | - |
| Merge: newest set per shortcode, tie smaller URL; `mine` | emoji-directory.ts:89-110 | DONE (emoji.rs:288-313) | - | - |
| Live set replacements | live.ts | DONE (live.rs:485; session/mod.rs:1190) | - | - |
| Lazy `ensure()` load | emoji-directory.ts:151-154, 292-303 | DONE (session/mod.rs `ensure_emoji`; `Session::ensure_emoji`, called when a community is shown) | - | - |
| Reconnect aborts in-flight read and restarts | emoji-directory.ts:285-291 | DONE (session/mod.rs `emoji_reconnect`) | - | - |
| Re-read after cache clear | emoji-directory.ts:262-280 | DONE (session/mod.rs `emoji_clear`) | - | - |
| Send-time `emoji` tags (errors, skip links, first match) | emoji-directory.ts:292-311; emoji.ts:81-113 | DONE (messages.rs:178-255) | - | - |
| Add emoji (fresh own-set read, `created_at = max(now, prev+1)`, 12 s, serialized, "Failed to add emoji." / "Timed out while adding emoji.") | emoji-directory.ts:156-227, 242-259 | DONE (session/account.rs `add_emoji`: newer of cached and fresh, reference error copy; the image link check is kept as input validation) | - | - |
| `suggestShortcode` | emoji.ts:19-30 | DONE (emoji.rs:145-160) | - | - |
| Reaction content rule | emoji.ts:6-10 | DONE (messages.rs:722-746) | - | - |
| Render custom emoji, never inside links | emoji.ts:81-100 | DONE (crates/app/src/ui/markdown.rs:168, 432) | - | - |
| Signer-side `validEmojiSetTemplate` | emoji.ts:32-67 | DONE (emoji.rs `valid_set_template`, checked before signing) | - | - |

### GIFs

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Capability gate (NIP-11 `buzz-gif`, `klipy`, `/gifs/search`) | gif-capability.ts:10-22; gifs.ts:162-192 | DONE (gifs.rs:19-25; session/gifs.rs:194-208) | - | - |
| HTTPS-only communities | gifs.ts:51-62 | DONE (session/gifs.rs:195, 218) | - | - |
| Per-community discovery cache, forgotten on failure | gifs.ts:156-177, 219-225 | DONE (crates/relay/src/session/gifs.rs) | - | S |
| Search body (customer_id, locale, trimmed query; fallback `buzz-desktop`) | gifs.ts:120-130, 206-216 | DONE (gifs.rs:28-35; crates/app/src/ui/gif_picker.rs:60-80) | - | - |
| Result normalization | gifs.ts:64-118 | DONE (gifs.rs:56-106) | - | - |
| Errors ("GIF request failed (status)", "GIF search failed") | gifs.ts:150-153, 217 | DONE (session/gifs.rs:224-236) | - | - |
| Markdown output | gifs.ts:229-237 | DONE (gifs.rs:110-122) | - | - |

### Reminders (kind 30300)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| List read (limit 200) with loading/ready/error and `hydrated` | reminders.ts:241-272 | DONE (crates/relay/src/reminders.rs, crates/relay/src/session/reminders.rs) | - | L |
| Self-decrypt with validation (one `d`, ≤16 KiB, target dialect, field limits, invalid skipped) | src-tauri/src/relay/reminders.rs:30-64, 94-146; reminders.ts:76-120 | DONE (crates/identity/src/reminders.rs via buzz-desktop relay::reminders) | - | M |
| Live 30300 on membership route | reminders.ts:314-320; live.ts:610-615 | DONE (crates/relay/src/session/reminders.rs) | - | S |
| Head merge (newest, then lowest id) | reminders.ts:172-183 | DONE (crates/relay/src/reminders.rs) | - | S |
| Create / snooze / complete / cancel (expiration 30-90 days) | reminders.ts:122-130, 273-298 | DONE (crates/relay/src/reminders.rs, crates/relay/src/session/reminders.rs) | - | M |
| Ordered edits per coordinate; monotonic created_at | reminders.ts:201-224 | DONE (crates/relay/src/session/reminders.rs) | - | S |
| Offline/recover gating | reminders.ts:148-153, 300-312 | DONE (crates/relay/src/reminders.rs) | - | S |
| Signer guard (exact tags, time window, expiration, NIP-44 to self) | src-tauri/src/relay/reminders.rs:66-92, 174-240 | DONE (crates/identity/src/reminders.rs: prepare, seal, admit) | - | S |
| Reminders page, due badge, alarm marker, `buzz-reminders-checked.v1` | docs/port/ui.md:859, 1344 | DONE (crates/app/src/ui/reminders.rs, crates/app/src/ui/reminders/) | - | L |

### Work sessions and channel creation

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Uncertain creation confirmed by strong id read | work-sessions.ts:60-113 | DONE (session/actions.rs:549-559; lifecycle.rs:671-680) | - | - |
| `delivered()` gives up after 15 s | work-sessions.ts:128-173 | DONE (crates/relay/src/session/work_sessions.rs) | Waits indefinitely while Sending (session/actions.rs:331-350) | S |
| Work-session create (private, parent validation, title 1-120) | work-sessions.ts:543-562 | DONE (crates/relay/src/session/work_sessions.rs) | Sessions only read (discovery.rs:640-667) | M |
| `invite` (top-level session, library agents) | work-sessions.ts:563-584 | DONE (crates/relay/src/session/work_sessions.rs) | - | S |
| `addAgents` (strong roster, parent cascade, archive/library gates, 15-min stale error) | work-sessions.ts:284-317, 388-494 | DONE (crates/relay/src/session/work_sessions.rs) | Single-channel `add_member` only (session/actions.rs:574-640) | M |
| Refresh gate with expected members/parent, 15 s | work-sessions.ts:185-283 | DONE (crates/relay/src/session/work_sessions.rs) | Polls channel state (session/actions.rs:316-330) | S |
| `memberChannels` (`#p` 39002 limit 500) | work-sessions.ts:320-353 | DONE (crates/relay/src/session/work_sessions.rs) | Needed by agent relay removal | S |
| `listsMember` exact roster read | work-sessions.ts:356-387 | DONE (crates/relay/src/session/work_sessions.rs) | - | S |
| `failed` / `discardFailed` for creation | work-sessions.ts:585-604 | DONE (crates/relay/src/session/work_sessions.rs) | No discard path for create | S |

### Workflow attribution

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `workflowOwner` rule | workflow-attribution.ts:5-26 | DONE (unread.rs:202-227; fold/mod.rs:112; session/incoming.rs:195) | - | - |
| Notification sender "Workflow · owned by X" | workflow-attribution.ts:29-31 | DONE (crates/app/src/notifications.rs:180-188) | - | - |
| Workflow rows never group | src/features/messages/message-grouping.ts:12-13 | DONE (crates/app/src/ui/message.rs:54-55) | - | - |
| Message row and thread byline (WorkflowByline) | MessageRow.tsx:231-250, 391, 585; ThreadPanel.tsx:338-343 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/thread.rs) | Render in message.rs / thread.rs | S |
| Inbox, popover, search, preview labels | InboxPage.tsx:626-705; SearchResults.tsx:552-562 | DONE (crates/app/src/ui/inbox.rs, crates/app/src/ui/workflows/, crates/app/src/ui/channel_activity.rs) | - | M |

### Product feedback (kind 42000)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Feedback event (`category`, text, attachments, limits) | product-feedback.ts:1-28 | DONE (crates/relay/src/feedback.rs) | - | - |
| Durable outbox send, availability gate, diagnostics upload | FeedbackDialog.tsx:60-140; session.ts:1723-1743 | DONE (crates/relay/src/session/feedback.rs, crates/app/src/ui/feedback.rs) | - | - |
| 42000 excluded from views | session.ts:292, 355, 461, 541 | DONE (threads.rs:111-112, 495) | - | - |

### Identity archives (13535)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Availability (authority = NIP-11 self) | identity-archives.ts:49-63 | DONE (crates/app/src/ui/agents/relay.rs, crates/app/src/ui/archives.rs) | - | S |
| Snapshot read (limit 1, one event, one `-` tag, ≤2 MiB) | identity-archives.ts:68-140 | DONE (crates/app/src/ui/agents/relay.rs `read_archives`) | - | M |
| Rollback fence | identity-archives.ts:57-59, 100-107 | DONE (crates/app/src/ui/agents/relay.rs, crates/app/src/ui/archives.rs) | - | S |
| `archiveHides` in mentions, agent choices, member add, sessions, search | identity-archives.ts:28-34 | DONE (crates/app/src/ui/archives.rs: mentions, member add, search authors; agent choices and sessions are the agents slice) | - | M |
| Consent path (self, owner/admin via 13534, NIP-OA `auth`) | identity-archives.ts:144-179 | DONE (crates/app/src/ui/agents/relay.rs `consent`) | - | M |
| 9035/9036 request (20 s timeout, re-read confirm) | identity-archive-protocol.ts:8-48; identity-archives.ts:180-233 | DONE (crates/app/src/ui/agents/relay.rs `request_archive`) | - | M |

### Threads and follows

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| View state machine (initial 10, page 50, ≤10 pages, 2000 events / 4 MiB) | threads.ts:18-33, 300-596 | DONE (threads.rs:95-121, 572-617; threads/window.rs:16-38) | - | - |
| Strict window verification | thread-window.ts:17-158 | DONE (threads/window.rs:197-359; threads.rs:792) | - | - |
| Legacy fallback; non-advancing pagination error | threads.ts:481-551 | DONE (threads.rs:~800-860) | - | - |
| Exact mode | threads.ts:328-395 | DONE (threads.rs:484-492, 598-616) | - | - |
| Live receive with 2-hop aux closure | threads.ts:119-150, 173-240, 264-271 | DONE (threads.rs:521-546) | - | - |
| Purge/revoke; post-revocation reads fenced | threads.ts:277-299 | DONE (threads.rs:548-568; session/threads.rs:136-162) | - | - |
| Repair on re-establish; coalesced refreshes | threads.ts:300-305, 590-593 | DONE (session/threads.rs:202-212; threads.rs:573-578) | - | - |
| 401/403 denies the channel | session.ts | DONE (session/threads.rs:156-161) | - | - |
| Cap of 64 owned thread views | session.ts:1785, 1934 | DONE (session/threads.rs) | - | - |
| `threadReference` | thread-reference.ts:1-13 | DONE (fold/thread.rs:18-39; unread.rs:140-158) | - | - |
| Follow storage (newest 1000, per partition) | thread-follows.ts:1-63 | DONE (store.rs:804-836; unread.rs:2531-2546) | - | - |
| Forget follows on leave | thread-follows.ts:65-68 | DONE (store.rs:397-409) | - | - |
| Follow/Unfollow thread action | unread.ts follow; docs/port/ui.md:1421 | DONE (crates/app/src/ui/message.rs, crates/app/src/ui/message_actions.rs) | - | S |
| Duplicate follows model | n/a | DONE (threads/follows.rs removed; follows live in unread.rs) | - | - |

### Typing

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| 8 s lifetime, 2 s quiet after message, 1024 cap | typing.ts:4-6, 67-88 | DONE (typing.rs) | - | - |
| Pulse scope validation, access check | typing.ts:89-124 | DONE (typing.rs) | - | - |
| Completion wins in batch; deadline expiry | typing.ts:31-66, 145-162 | DONE (typing.rs; session/presence.rs:32-47) | - | - |
| Completion evidence from every admitted read | session.ts:551 | DONE (session/mod.rs `accept`) | - | - |
| Clear on revocation and cache clear | session.ts:369, 2617 | DONE (session/mod.rs `revoke_access`; session/channels.rs `clear_cache`) | - | - |

### User status (30315)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Parse rules (`d=general`, ≤100 UTF-16, expiration) | user-status.ts:17-41; user-status-policy.ts:1-11 | DONE (user_status.rs:91-115) | - | - |
| Save (fresh read, monotonic, 300 s skew, 12 s, one at a time) | user-status.ts:208-284 | DONE (user_status.rs:120-158; session/account.rs:155-189) | - | - |
| Writable gate | user-status.ts:209-215, 329-332 | DONE (session/account.rs `set_status`) | - | - |
| Watched directory for others (refcounted, batches of 100) | user-status.ts:123-207 | DONE (user_status.rs `StatusDirectory`; session/statuses.rs `Session::watch_statuses`) | - | - |
| Live 30315 updates | user-status.ts:96-122 | DONE (session/mod.rs `accept`, through `StatusDirectory::accept`) | - | - |
| Expiry timer republish | user-status.ts:68-95 | DONE (user_status.rs `StatusDirectory::deadline`; session/statuses.rs `statuses_tick`) | - | - |
| Background retries 5/15/60 s | user-status.ts:149-161 | DONE (user_status.rs `StatusDirectory::finished`, `next_read`) | - | - |
| `current(userId)` for others; reload on reconnect/clear | user-status.ts:310-350 | DONE (`Session::user_status`; session/statuses.rs `statuses_reconnected`, `statuses_clear`) | - | - |
| Status on profiles, member lists, rows | UI consumers | DONE (crates/app/src/ui/profile_panel.rs; sidebar DM rows follow `Session::watch_statuses` in crates/app/src/ui/sidebar.rs). buzz-app shows no status in member lists | - | M |

### Me preferences (Me groups kit)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Me kit record read/decode/save with revision checks | me-preferences.ts:72-123 | DONE (crates/relay/src/kit.rs, crates/relay/src/session/kit.rs) | Kit owner | L |
| `placement.has/admit/set` with capacity message | me-preferences.ts:42-69, 198-240 | DONE (crates/relay/src/session/kit.rs) | - | M |
| Section create/assign/delete through sidebar store | me-preferences.ts:124-186 | DONE (crates/relay/src/session/kit.rs) | - | M |
| Projection: personal groups replace legacy sections; Me channels leave main sidebar | sidebar-personal-groups.ts | DONE (crates/relay/src/session/sidebar.rs, crates/app/src/ui/sidebar.rs) | Pure model only (sidebar.rs:807-840) | S |

### Native agent operations and project Git

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `relay_agent_resolve` owner proof | src-tauri/src/relay/agent.rs:45-58 | DONE (crates/identity/src/lib.rs `resolve_agent_community` with buzz-desktop `relay::agent::resolve`) | - | S |
| `relay_agent_log_proof` | agent.rs:68-112 | DONE (crates/identity/src/lib.rs `harness_log_proof` with buzz-desktop `relay::agent::log_proof`) | - | S |
| Observer telemetry 24200 decrypt | agent.rs:124-232; native.ts:612-628 | DONE (crates/app/src/agent_activity/observer.rs, crates/identity/src/lib.rs `decode_agent_frame`) | - | M |
| Archive 44200 decode and archive settings | agent.rs:203-232; native.ts:596-640 | DONE (crates/identity/src/lib.rs `decode_agent_frame`, `decode_archive_page`; crates/app/src/ui/settings/archive.rs) | - | M |
| Agent memories 30174 read (limits, HMAC d-tag, newest per slug, `MemoryDenied`) | agent.rs:317-497; native.ts:666-690 | DONE (crates/relay/src/session/memories.rs `Session::agent_memories`; crates/identity `decode_agent_memories` with buzz-desktop's decoder) | - | L |
| `relay_agent_library` (legacy managed-agents.json listing) | agent.rs:499-636 | DIFFERENT | crates/host/src/import.rs:51-80 reads it for import only | M |

## Channel page, settings, lifecycle, create, canvas, templates and usage

The relay side of lifecycle and details editing matches well (templates, permissions, owner-agent evidence, confirmation polling). The UI is much thinner: no tab pane, settings panel, duration control, privacy confirmation, uncertain-save recovery, Canvas, templates or usage view. Paths: buzz-app `src/bundled/channels/` (BC), `src/features/relay/` (FR), `src/features/channel-templates/` (FT), `src/bundled/channel-templates/` (BT), `src/bundled/channel-usage/` (BU); port `crates/relay/src/` (R) and `crates/app/src/ui/` (A).

### Channel page layout and states

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Disconnected/error splash ("Your channels, one conversation.") | BC/ChannelsPage.tsx:128-148 | DONE (crates/app/src/ui/channel_states.rs): for a community that is not connected. No community still goes to onboarding (shell.md's Personal space row). | No community goes to onboarding (A/workspace.rs:145-160); no splash for disconnected/failed community | S |
| Default selection: saved `selected-channel`, else first non-session channel | BC/ChannelsPage.tsx:207-209, 348-356, 1495 | DONE (crates/app/src/ui/channel_states.rs, crates/app/src/ui/workspace.rs) | Only remembered channel restored (A/workspace.rs:179-202); else empty text (A/workspace.rs:459-472) | S |
| Routed unjoined channel: exact `resolve()` preview, "Checking conversation access…" | BC/ChannelsPage.tsx:258-296, 1486-1497 | DONE (crates/app/src/ui/workspace.rs, crates/app/src/ui/channel_states.rs) | Waits for roster then logs and drops (A/workspace.rs:661-672, 693-718); search already resolves (A/search.rs:692) | M |
| "Loading messages…" placeholder | BC/ChannelBody.tsx:96-105 | DONE | - | - |
| Timeline error with "Retry messages" | BC/ChannelBody.tsx:81-95 | DONE (crates/app/src/ui/channel.rs) | Error text only (A/channel.rs:716) | S |
| Header: channel tab, "Channel members", "Channel actions" | BC/ChannelsPage.tsx:1301-1405 | DONE | - | - |
| Read-only preview notice and "Join channel" | BC/ChannelJoinNotice.tsx:9-62; BC/ChannelsPage.tsx:1499-1525 | DONE (crates/app/src/ui/channel.rs, crates/app/src/ui/channel_menu.rs): the composer cannot send; its input stays editable, as the composer has no disabled state. | Also requires `visibility==Public` (A/channel_menu.rs:631-635); replaces composer instead of sitting above it (A/channel.rs:579-584) | S |
| Join outcome ("Join may have taken effect… Try again to check.") and composer focus | BC/ChannelJoinNotice.tsx:31-48; BC/ChannelsPage.tsx:363-371 | DONE (crates/app/src/ui/channel_menu.rs, crates/app/src/ui/channel.rs) | Generic UNCONFIRMED text (R/session/actions.rs:35); no composer focus (A/channel_menu.rs:605-626) | S |
| After Leave/Delete/Hide select next row; Archive keeps page | BC/ChannelsPage.tsx:340-347 | DONE (A/channel_menu.rs:127-133; A/workspace/dialogs.rs:126-142) | - | - |
| Settings "Diagnostics" (UnreadOptions, "Partial roster · N channels", Refresh channels / groups and stars / messages, Outbox, timings) | BC/ChannelsPage.tsx:1163-1200 | DONE (crates/app/src/ui/channel_diagnostics.rs, crates/app/src/ui/outbox_status.rs, crates/app/src/ui/channel_settings.rs). The roster line always reads "Roster", as the session does not publish partial coverage; there are no timings (RelayTimings) | - | M |

### Conversation tabs, picker and panels

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Toggle tab pane" button, tab close with fallback and focus return | BC/ChannelsPage.tsx:1406-1426; BC/useConversationTabs.tsx:297-321 | DONE (crates/app/src/ui/channel.rs, crates/app/src/ui/workspace/board.rs, crates/app/src/ui/channel_tabs.rs) | Tab pane workspace | M |
| Tab state per session and channel kept across visits | BC/useChannelTabState.ts:1-164 | DONE (crates/app/src/ui/channel_tabs.rs, crates/app/src/ui/workspace/board.rs) | One thread panel per workspace, cleared on channel switch (A/workspace.rs:233, 257-308; A/thread.rs:556-587) | M |
| "New tab" picker (Channels/DMs/Tools, fuzzy, usage-ranked, keyboard) | BC/ChannelTabPicker.tsx:49-229 | DONE (crates/app/src/ui/channel_tab_picker.rs) | - | L |
| Secondary conversation tabs and thread tabs ("This conversation is no longer available.") | BC/ConversationTab.tsx:1-181; BC/useConversationTabs.tsx:322-379, 430-486 | DONE (crates/app/src/ui/workspace/board.rs) | - | L |
| Plugin channel launchers, drawer and side panel | BC/useChannelPanels.tsx:1-141 | DONE (crates/app/src/ui/channel_panels.rs, crates/app/src/ui/workspace/board.rs; slices register their panels) | Plugin panel host | L |
| Channel-menu plugin panels (e.g. "View channel usage") as tabs | BC/ChannelsPage.tsx:1357-1404; BC/useConversationTabs.tsx:153-170, 487-504 | DONE (crates/app/src/ui/channel_panels.rs, crates/app/src/ui/channel_menu.rs, crates/app/src/ui/workspace/board.rs) | - | M |
| Link routing into panels; legacy session links into a tab | BC/ChannelsPage.tsx:955-1050 | DONE (crates/app/src/ui/workspace/board.rs `route_link_to_tab`) | `buzz://` navigation only (A/workspace.rs:789-817) | M |

### Channel settings panel and details editor

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "Channel settings" tab (identity, Escape closes, "Editing is unavailable on this connection.") | BC/ChannelSettingsPanel.tsx:31-224; BC/ChannelsPage.tsx:1561-1585 | DONE (crates/app/src/ui/channel_settings.rs, crates/app/src/ui/workspace/board.rs; every port session can sign, so "Editing is unavailable on this connection." never applies) | Modal "Channel details" dialog (A/channel_menu.rs:379-581) | M |
| Description and Visibility rows (hidden for DM/session), click to edit | BC/ChannelSettingsPanel.tsx:103-181 | DONE (crates/app/src/ui/channel_settings.rs) | Fields shown directly, disabled for non-editors; Description shown for DMs (A/channel_menu.rs:485-546) | S |
| Members row with count; Channel ID row with copy ("Channel ID copied") | BC/ChannelSettingsPanel.tsx:182-192, 308-356 | DONE (crates/app/src/ui/channel_settings.rs) | Port has "Copy link" instead (A/channel_menu.rs:157-171) | S |
| Setup tools (Save as template, "Personal group" select, lifecycle buttons) | BC/ChannelsPage.tsx:1092-1158 | DONE (crates/app/src/ui/channel_settings.rs, crates/app/src/ui/personal_group.rs, crates/app/src/ui/templates/save_as.rs) | - | M |
| Personal groups settings card ("Manage personal groups", "Reload groups") | BC/ChannelSetupSettings.tsx:1-105 | DONE (crates/app/src/ui/settings/personal_groups.rs) | - | M |
| Fresh load, reload keeping explicit choices, "Reload details" on error | BC/ChannelDetailsEditor.tsx:122-168, 292-301 | DONE (crates/app/src/ui/channel_settings.rs) | Loads once; error replaces body, no reload (A/channel_menu.rs:405-423, 484) | S |
| Save gating (dirty and valid) and inline field errors | BC/ChannelDetailsEditor.tsx:173-197, 492-521; FR/channel-details-protocol.ts:45-76 | DONE (crates/app/src/ui/channel_settings.rs) | Save always enabled; errors only after submit (A/channel_menu.rs:436-449, 565-577; R/lifecycle.rs:613-633) | S |
| "Discard changes?" (Keep editing / Discard changes) | BC/ChannelDetailsEditor.tsx:218-234, 384-395 | DONE (crates/app/src/ui/channel_settings.rs) | - | S |
| Privacy change confirmation ("Make channel private?/public?") | BC/ChannelDetailsEditor.tsx:396-441, 474-479 | DONE (crates/app/src/ui/channel_settings.rs, crates/app/src/ui/form_fields.rs) | - | S |
| Uncertain save survives reopen ("Check save status", "Check the previous change before saving again.") | FR/channel-details.ts:31-32, 142-153, 162-163, 206-211; BC/ChannelDetailsEditor.tsx:258-303, 450-458 | DONE (crates/app/src/ui/channel_settings.rs, crates/relay/src/session/actions.rs) | Port returns UNCONFIRMED and allows immediate resave (R/session/actions.rs:511-540) | M |
| Normalization and copy | BC/ChannelDetailsEditor.tsx:173-176 | DONE (crates/app/src/ui/channel_settings.rs) | Port trims description (A/channel_menu.rs:442); shows "This channel's details can't be changed now." for any TTL channel even when the reason is role (A/channel_menu.rs:547-555) | S |

### Shared form fields

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| ChannelTextField (counter at ≥90 %, input bounded by code points, SR count) | BC/ChannelTextField.tsx:22-34, 81-98 | DONE (crates/app/src/ui/form_fields.rs) | Always shows N/limit and allows overflow (A/new_channel.rs:121-136; A/channel_menu.rs:486-525) | S |
| ChannelDurationField (Ongoing/Temporary, 1d/1w/2w steps) | BC/ChannelDurationField.tsx:8-131 | DONE (crates/app/src/ui/form_fields.rs) | - | M |
| ChannelPrivacyConfirmation with "Don't show me this again" per scope | BC/ChannelPrivacyConfirmation.tsx:6-49 | DONE (crates/app/src/ui/form_fields.rs) | - | S |

### Details protocol

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| `channelVisibility` and `canonicalDetailsName` | FR/channel-details-protocol.ts:26-42 | DONE (R/discovery.rs:180; R/lifecycle.rs:607-611) | - | - |
| Strong 39000/39001/39002 read, canEdit, TTL parse | FR/channel-details-protocol.ts:98-134, 181-186; FR/channel-details.ts:82-101 | DONE (R/lifecycle.rs:134-152, 194-207, 383-407; buzz-app also rejects non-members) | - | - |
| 9002 template (visibility/ttl only when changed) | FR/channel-details-protocol.ts:136-178 | DONE (R/lifecycle.rs:709-744) | - | - |
| Session channels: name-only 9002 `[h,name]` | FR/channel-details-protocol.ts:78-96, 142-154, 189-212 | DONE (lifecycle.rs `edit_details`, `validate_details`) | - | - |
| Authorize before and after signing; error copy | FR/channel-details.ts:166-200 | DONE (session/actions.rs `authorize_details`) | - | - |
| Confirm by re-read and adopt metadata | FR/channel-details.ts:102-120 | DONE (R/session/actions.rs:534-540; R/lifecycle.rs:747-761) | - | - |
| Access gate ("Channel access changed. Reopen channel settings.") and 20 s timeout | FR/channel-details.ts:51-81 | DONE (session/actions.rs `read_details`; 20 s in `Session::run`) | - | - |

### Lifecycle UI

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Header lifecycle menu (Hide / Leave / Archive / Unarchive / Delete check unavailable / Delete) | BC/ChannelLifecycleMenu.tsx:22-168 | DONE (crates/app/src/ui/channel_menu.rs) | Items there (A/channel_menu.rs:102-116, 179-255); missing "Retry channel permissions" / "Retry Delete check" and "unavailable on this connection"; port shows "Checking…" | S |
| Settings-panel lifecycle buttons with retry, "No channel actions available." | BC/ChannelLifecycleActions.tsx:15-162 | DONE (crates/app/src/ui/channel_settings.rs) | - | S |
| Confirmation dialog copy, Delete destructive, no close while busy | BC/ChannelLifecycleDialog.tsx:10-36, 82-118 | DONE (A/channel_menu.rs:259-313, 331-345) | - | - |
| After unconfirmed result, confirm stays disabled | BC/ChannelLifecycleDialog.tsx:56-76, 104 | DONE (crates/app/src/ui/channel_settings.rs `LifecycleDialog`, crates/relay/src/lifecycle.rs `ActionError::Unconfirmed`) | Confirm stays enabled, action can be resent (A/channel_menu.rs:300-310) | S |

### Lifecycle protocol

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Templates archive/unarchive 9002, delete 9008, join 9021, leave 9022, hide 41012 | FR/channel-lifecycle-protocol.ts:23-73 | DONE (R/lifecycle.rs:490-513) | - | - |
| Permissions incl. last-owner leaveReason | FR/channel-lifecycle-protocol.ts:110-180 | DONE (R/lifecycle.rs:212-290, 330-351; port also cross-checks 39001 vs 39002) | - | - |
| Owner-agent evidence (kind 0, batches of 4, 5 s) | FR/channel-lifecycle.ts:133-218 | DONE (R/session/actions.rs:405-447) | - | - |
| Re-authorize after signing, single in-flight guard, error copy | FR/channel-lifecycle.ts:286-363 | DONE (session/actions.rs `authorize_lifecycle`; `Session::channel_action`) | - | - |
| Join preconditions | FR/channel-lifecycle.ts:302-320 | DONE (lifecycle.rs `join_ready`) | - | - |
| Confirmation polling 0/250/750/1500 ms | FR/channel-lifecycle.ts:369-424 | DONE (R/lifecycle.rs:23-28, 516-569; R/session/actions.rs:484-509) | - | - |
| Unconfirmed error text with reason | FR/channel-lifecycle.ts:20-27, 426-428 | DONE (lifecycle.rs `ActionError::Unconfirmed`; session/actions.rs `channel_action`) | - | - |
| DM visibility snapshot (status, newer wins, invalid list = error) | FR/channel-lifecycle.ts:219-282 | DONE (lifecycle.rs `visibility_record`; session/actions.rs `refresh_visibility`; `SessionSnapshot::dm_visibility`) | - | - |

### Create channel

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Name/description, canonical name, Private switch, Create disabled when invalid | BC/CreateChannelDialog.tsx:133-184, 379-412, 446-465 | DONE (crates/app/src/ui/new_channel.rs) | Works (A/new_channel.rs:63-113, 168-185); no inline errors or "Buzz session (" pre-check | S |
| Temporary duration (ttl) | BC/CreateChannelDialog.tsx:135-138, 225, 550-560 | DONE (crates/app/src/ui/new_channel.rs, crates/relay/src/lifecycle.rs) | NewChannel has no ttl (R/lifecycle.rs:636-668) | M |
| Privacy confirmation and "Discard changes?" | BC/CreateChannelDialog.tsx:186-199, 341-399 | DONE (crates/app/src/ui/new_channel.rs) | (A/new_channel.rs:140-144, 168-178) | S |
| Pending creation recovery ("Retry channel", restore after restart) | BC/CreateChannelDialog.tsx:157-175, 411, 434-439; FT/setup.ts:130-190 | DONE (crates/app/src/ui/new_channel.rs, crates/relay/src/session/kit.rs) | - | M |
| Group destination ("Create a channel in <group>") | BC/CreateChannelDialog.tsx:142-145, 207-216, 311-327 | DONE (crates/app/src/ui/new_channel.rs, sidebar/rows.rs); the templates setup summary belongs to the templates slice | - | M |
| Template/teams editor and setup summary | BC/CreateChannelDialog.tsx:253-282, 466-549, 562 | DONE (crates/app/src/ui/templates/editor.rs, crates/app/src/ui/new_channel.rs) | - | L |
| 9007 via outbox, receipt, roster confirm, open channel; gated on relay support | docs/port/relay.md §5m.1 | DONE (R/session/actions.rs:542-572; A/new_channel.rs:79-113; A/workspace.rs:486-489) | - | - |

### Canvas

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "View canvas" and Canvas dialog (Markdown, saved draft `canvas-draft-v1`, "Save Canvas") | BC/ChannelCanvasDialog.tsx:13-243; BC/ChannelHeaderMenu.tsx:99-108 | DONE (crates/app/src/ui/canvas/mod.rs, crates/app/src/ui/channel_menu.rs) | - | L |
| Conflict handling ("Canvas changed since this draft began…", "Discard and reload") | BC/ChannelCanvasDialog.tsx:58-124, 155-166, 248-272; FT/canvas-conflict.ts:1-22 | DONE (crates/app/src/ui/canvas/mod.rs, crates/relay/src/session/canvas.rs) | - | M |
| Save protocol (40100 `expected-revision`, 24 KiB, 1 s spacing) | FT/capability.ts:426-528; FT/canvas-signing-contract.json | DONE (crates/relay/src/canvas.rs, crates/relay/src/session/canvas.rs) | - | M |
| History (paged strong read of 25, restore confirm, >24 KiB not restorable) | BC/CanvasHistory.tsx:26-315; FT/capability.ts:443-486 | DONE (crates/app/src/ui/canvas/history.rs, crates/relay/src/canvas.rs) | - | M |

### Channel completion (#)

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Joined-channel completion (tiered match, limit 20, escaped link, "Private channel") | BC/ChannelCompletion.tsx:11-37, 48-55, 124-145 | DONE (crates/app/src/ui/composer/channels.rs) | Mostly done (A/composer.rs:805-845, 1379-1391); missing newline-name filter, membership/cached check, id tiebreak | S |
| Open unjoined channels ("Not joined", 5 rows reserved) | BC/ChannelCompletion.tsx:38-41, 67-117 | DONE (crates/app/src/ui/composer/channels.rs) | - | M |
| Status/retry lines, duplicate-name id detail, canSelect recheck | BC/ChannelCompletion.tsx:87-92, 119-176 | DONE (crates/app/src/ui/composer/channels.rs) | - | S |

### Live status and relay timings

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|

### Channel templates

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Plugin registration and gating (Templates card, providers) | BT/index.tsx:1-28; FT/provider.ts:1-88 | DONE (crates/app/src/ui/templates/mod.rs) | - | M |
| Channel kit store (30078 NIP-44, 16 KiB, decode checks, model validation) | FT/capability.ts:50-425; FT/model.ts:13-291; FT/host.ts | DONE (crates/relay/src/kit/mod.rs, crates/relay/src/session/kit.rs; record rules from buzz-desktop `relay::kit`) | - | L |
| Template library and dialog | BT/TemplateLibrary.tsx; BT/TemplateSettings.tsx; BT/ChannelTemplatesDialog.tsx | DONE (crates/app/src/ui/templates/library.rs, crates/app/src/ui/templates/dialog.rs) | - | L |
| Template selection and fields (agent search, teams, Starting Canvas, group default) | BT/TemplateEditor.tsx:1-270; BT/TemplateFields.tsx:1-232 | DONE (crates/app/src/ui/templates/fields.rs, crates/app/src/ui/templates/editor.rs; group defaults in crates/app/src/ui/settings/personal_groups.rs) | - | M |
| Portable team payload (16 KiB chunks, 8 MiB) | FT/team-payload.ts:1-180 | DONE (crates/relay/src/kit/payload.rs) | - | M |
| Setup orchestration (`buzz-channel-setup.v2`) | FT/setup.ts:46-409 | DONE (crates/relay/src/kit/setup.rs, crates/relay/src/session/kit.rs; Canvas seed through crates/relay/src/canvas.rs) | - | L |
| Save channel as template; catalog hook | BT/TemplateSettings.tsx:80+; BT/useTemplateCatalog.ts:1-38 | DONE (crates/app/src/ui/templates/save_as.rs) | - | M |

### Channel usage

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| "View channel usage" panel and usage archive read (2,000-record cap) | BU/index.tsx:7-46; BU/use-usage-archive.ts:1-160 | DONE (crates/app/src/ui/channel_usage.rs, crates/relay/src/usage.rs, crates/relay/src/archive.rs) | - | M |
| Usage view (agent/session pickers, totals, tokens, cache, USD, per-turn list) | BU/ChannelUsage.tsx:1-301 | DONE (crates/app/src/ui/channel_usage.rs) | - | L |

## buzz-app main of 2026-10-14

| Feature | buzz-app reference | Port status | What's needed | Size |
|---|---|---|---|---|
| Templates & teams plugin removed: Templates card, template selector and group default in Create a channel, Save as template in the channel menu and settings, group default template in Personal groups; copy "Personal organization only. Moving channels never changes their members or Canvas.", "…will move back to Channels. This does not delete any channels.", "…It will not create another channel or continue its setup." (#859 `29c36a8b`) | src/bundled/channel-templates/**; src/bundled/channels/{CreateChannelDialog,ChannelHeaderMenu,ChannelsPage,ChannelGroupsDialog,SidebarSection}.tsx; src/features/channel-templates/capability.ts | DONE (removed ui/templates; ui/new_channel.rs keeps setup notices and the recovery summary; ui/channel_menu.rs; ui/channel_settings.rs; ui/settings/personal_groups.rs; ui/sidebar/dialogs.rs) | A group's saved default stays in its record. The relay layer's template, team payload and team text code is left unused, to keep this sync's churn in the relay crate small | M |
| Channel settings and member administration read the channel type from the first `t` tag of relay-signed channel state; later `t` tags are labels; a malformed first `t` is "Malformed channel t state" (#958) | src/features/relay/channel-lifecycle-protocol.ts `channelTypeTag`; src/features/channel-members/administration-protocol.ts | DONE (relay/src/lifecycle.rs `channel_type_tag`) | — | S |
| Personal groups removed: the sidebar always reads, moves into and removes its own sections; Settings → Personal groups and the channel settings "Personal group" picker are gone; a new channel in a group no longer applies a personal group's default template; only the Me groups record parses ("Too many Me groups", "Duplicate Me group", "Invalid Me group placement"), and saved `groups:personal` records are skipped before decode so they never reject the catalog. No migration (#988) | src/features/relay/{session,sidebar-preferences-store,sidebar-preferences,me-preferences}.ts; src/features/channel-templates/{capability,model,setup}.ts; src/bundled/channels/{index,ChannelsPage,CreateChannelDialog}.tsx; src/features/sessions/workspace.ts | DONE (relay/src/kit/{mod,me,setup}.rs, relay/src/session/{me_kit,sidebar,kit}.rs, relay/src/sidebar.rs `MeGroups`; app: ui/personal_group.rs and ui/settings/personal_groups.rs deleted, ui/channel_settings.rs, ui/new_channel.rs, ui/templates/editor.rs, ui/me/settings.rs) | The native kit validator narrowing (`src-tauri/src/relay/kit.rs`) is in buzz-desktop and follows the fork pin; the port never writes a `personal` record | M |
| Live sockets keep retrying transient drops and authentication timeouts with equal jitter (half to all of 500 ms doubling to a 30 s cap); only a full minute authenticated resets the backoff, a brief authentication does not; refusals stay terminal until Retry (#996) | src/features/relay/live.ts | DONE (relay/src/socket.rs `SocketOptions::{healthy_after, jitter}`) | The port already retried without a cap; it reset the backoff on any authentication | S |
| Inbox archive: Archive/Restore from a row's hover button, its context menu ("Archive conversation"/"Restore conversation") and the detail header; the filter menu's Type, From and Status (Inbox/Archived) groups, then Drafts; summary such as "Mentions · Archived · Agents"; archiving the open conversation moves to the next row (or the one before), reading it, else closes; Restore keeps the reader. A participating-thread reply or DM message at or after the archive's watermark reopens it (saved), Mentions needs a fresh explicit mention, which retires the archive; a regrouped conversation keeps its archive. Archived empty states and "Show all archived conversations"; save errors with Retry keeping the cutoff. Archives and filter choices saved on this device per account and community (#677) | src/bundled/inbox/{archive.ts,InboxPage.tsx,InboxFilters.tsx,InboxDetail.tsx}; src/features/relay/{inbox,unread}.ts | DONE (app/src/ui/inbox/archive.rs, ui/inbox.rs, ui/inbox/{model,detail}.rs; relay/src/unread.rs `InboxItem::messages`; ui/src/components/menu.rs `Menu::tall`) | One process, so no cross-window storage events (a guarded write still reports "Inbox archive changed in another window. Try again."); row buttons show on hover only (no keyboard focus model or touch); focus does not move after row actions | L |
| The personal space's sidebar uses the community sidebar's frame, scrollport, list and page-row insets, as do its connecting and error states, so switching spaces moves nothing (#945) | src/features/channel-navigation/{SidebarList,ChannelSidebar}.tsx; src/bundled/channels/Channels.module.css | DONE (app/src/ui/sidebar.rs `personal`, `scrollport`, `list`, `page_list`; ui/workspace/shell.rs) | The port's sidebar draws no Buzz mark | S |
| NIP-CL channel labels, creator and creation time on channel summaries; create with labels; details saves replace or clear labels and send none when unchanged; `find_labeled(creator, label)` oldest first with relay-signed creator/label checks; refused with "This community's relay does not support channel labels yet." unless NIP-11 lists `nip-cl` (#964) | src/features/relay/{channel-labels,channel-details,channel-details-protocol,store,session,discovery,native,work-sessions}.ts | DONE (relay/src/channel_labels.rs, discovery.rs `ChannelSummary::{labels,creator,created_at}`, lifecycle.rs, kit/setup.rs, session/actions.rs `find_labeled`, session/kit.rs, session/mod.rs `RelayInfo::channel_labels`) | No UI sends labels yet (upstream's neither). buzz-desktop's `channel_writes` allowlist change is not needed: the port signs its own 9002/9007 | M |
| IFC policy library and session APIs (#835) | crates/buzz-ifc, crates/ifc-core | N/A (library crates only; nothing in buzz-app's src/ or src-tauri uses them) | — | — |
| A reopened New message picker searches the relay again, so people added after the directory completed are found (#1044) | src/features/direct-messages/usePeople.ts | N/A: the port keeps no session directory cache; every visit's search reads the relay | — | — |
| Channel labels and identity name scopes are memoized (#939) | src/bundled/channels/useChannelLabels.ts; src/features/identity-names/service.ts | N/A: a React render cost | — | — |
| A channel's threads are tabs of their own: opening one selects its tab wherever it was placed or adds one; main-pane thread tabs follow the conversation in the channel's strip and show in its place; tabs are labelled with the start of the root's text, else "Thread · channel". History visits switch tabs and never close them (each applied once); a thread hint is verified by the reader, a wrong hint merging into the tab already holding that thread and a message of another thread opening its own tab while the first keeps its reply (`af303dff`) | src/bundled/channels/{useChannelTabState.ts,useConversationTabs.tsx,ChannelsPage.tsx,TabHosts.tsx}; src/features/panels/PanelWorkspace.tsx; src/features/messages/ThreadPanel.tsx | DONE (app/src/ui/channel_tabs.rs, ui/channel_tab_views.rs, ui/workspace/board.rs, ui/workspace.rs, ui/navigation.rs `Beside`, ui/thread.rs, ui/me/board.rs) | Not ported: Alt+Shift+Left/Right moving a focused tab and its "Moved to …" announcement; the port's tabs take no keyboard focus (their content does) and GPUI has no live region | L |
| Dragging a thread tab by its title moves it within or between the main and side strips, with a drop line, edge separation into a closed or empty side pane, auto-scroll near a strip's ends, Escape or resize cancelling; another conversation's thread can only go to the side pane; navigation tabs as wide as their title up to 192px (`ebadc94c`) | src/features/panels/useTabDrag.tsx; src/shared/design-system/ui/{TabDragFeedback,Tabs}.tsx; styles/tab-drag.css | DONE (app/src/ui/tab_drag.rs, ui/workspace/board.rs, ui/me/board.rs, ui/panels.rs, ui/src/components/tabs.rs) | — | M |
| Closing an on-screen thread tab of the channel being read washes its root's row once with the accent if it is in view (1.1s, held a third, then fading; held then gone with reduced motion); nothing scrolls (`0de0f26f`) | src/features/messages/use-return-highlight.ts; Messages.module.css; src/bundled/channels/useConversationTabs.tsx | DONE (app/src/ui/channel.rs, ui/channel_tabs.rs, ui/message.rs, ui/workspace/board.rs) | — | S |
| Choosing a conversation or thread tab, in either pane, puts the focus in its composer with the caret where it was; tabs without a composer take the focus themselves (#909) | src/features/messages/EditableInput.tsx; MessageComposer.tsx; src/features/panels/PanelWorkspace.tsx; src/bundled/channels/ChannelsPage.tsx | DONE (app/src/ui/workspace/board.rs `focus_tab`, ui/me/board.rs) | — | S |
| A thread link whose hint named an existing tab but whose message is in another thread opens the corrected thread in that tab's own pane, not the default pane (#927) | src/bundled/channels/useChannelTabState.ts `divertThread` | DONE (app/src/ui/channel_tabs.rs `divert_thread`) | — | S |
| A moved thread tab takes the focus where it landed in the same update as its placement (#934) | src/bundled/channels/useConversationTabs.tsx `placeTab` | DONE (no change: a drop moves the tab and focuses its view in one update, app/src/ui/workspace/board.rs, ui/me/board.rs) | Keyboard moves are not ported (see the `af303dff` row) | — |
| Channel templates kept on this device (`buzz.channel-templates.v1`): name, agents, teams and a starting Canvas; Settings → Channel templates creates, edits and deletes them (#867 `146caad4`, from #868) | src/features/channel-templates/local.ts; src/bundled/channels/ChannelTemplateSettings.tsx | DONE (app/src/channel_templates.rs, ui/settings/templates.rs) | One process owns the file, so there is no other-window refresh | M |
| Create channel's Template select sends the channel setup: the template's agents, then its teams' members who are the owner's agents in this community, each once, plus the Canvas; deleted teams are skipped; a template that adds nothing makes a plain channel (#867, #900 `df03af99`) | src/bundled/channels/CreateChannelDialog.tsx; channel-templates/local.ts `templateAgents` | DONE (ui/new_channel.rs, app/src/channel_templates.rs `template_agents`) | — | S |
| Members search matches teams by name; "Add team" adds every member who is an agent in this community and not in the channel, through per-member additions with their own Retry (#867) | src/bundled/channels/ChannelMembersDialog.tsx | DONE (ui/members.rs) | — | S |
| Templates pick individual agents too: an Agents checklist above Teams, older records read as none, the summary names agents and teams, agents saved from another community are kept on edit (#900) | src/bundled/channels/ChannelTemplateSettings.tsx; channel-templates/local.ts | DONE (ui/settings/templates.rs, app/src/channel_templates.rs) | — | S |
| Each agent's avatar beside its name in the template editor, squircle with initials (#1013 `f442ed0a`) | src/bundled/channels/ChannelTemplateSettings.tsx | DONE (ui/settings/templates.rs, ui/src/components/checkbox.rs `label_element`) | — | XS |
| Relay leftovers of the v1 removal: the community agent catalog reader and publisher, the outbox's catalog rules and the signer's catalog purpose, saved-team writes and portable team payload/text encoding, the agent-management observer tap (#859) | FA/{catalog,catalog-protocol,management-request}.ts; channel-templates/team-payload.ts | DONE (removed relay/src/{catalog,catalog_tests,teams}.rs, relay/src/session/{catalog,teams}.rs, relay/tests/catalog_relay.rs; trimmed relay/src/kit/payload.rs, session/observer.rs, outbox.rs, identity) | Kit record parsers and the team-text admission corpus stay, as upstream | M |

## DONE count

**DONE: 547** of 562 rows.

Other statuses: PARTIAL 0, DIFFERENT 11, MISSING 0, SKIPPED 4.
